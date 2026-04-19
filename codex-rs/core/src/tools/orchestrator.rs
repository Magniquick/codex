/*
Module: orchestrator

Central place for approvals + sandbox selection + retry semantics. Drives a
simple sequence for any ToolRuntime: approval → select sandbox → attempt →
retry with an escalated sandbox strategy on denial (no re‑approval thanks to
caching).
*/
use crate::guardian::guardian_rejection_message;
use crate::guardian::guardian_timeout_message;
use crate::guardian::new_guardian_review_id;
use crate::guardian::routes_approval_to_guardian;
use crate::hook_runtime::run_permission_request_hooks;
use crate::network_policy_decision::network_approval_context_from_payload;
use crate::tools::network_approval::DeferredNetworkApproval;
use crate::tools::network_approval::NetworkApprovalMode;
use crate::tools::network_approval::begin_network_approval;
use crate::tools::network_approval::finish_deferred_network_approval;
use crate::tools::network_approval::finish_immediate_network_approval;
use crate::tools::sandboxing::ApprovalCtx;
use crate::tools::sandboxing::ExecApprovalRequirement;
use crate::tools::sandboxing::SandboxAttempt;
use crate::tools::sandboxing::SandboxOverride;
use crate::tools::sandboxing::ToolCtx;
use crate::tools::sandboxing::ToolError;
use crate::tools::sandboxing::ToolRuntime;
use crate::tools::sandboxing::default_exec_approval_requirement;
use codex_hooks::PermissionRequestDecision;
use codex_otel::ToolDecisionSource;
use codex_protocol::error::CodexErr;
use codex_protocol::error::SandboxErr;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::NetworkPolicyRuleAction;
use codex_protocol::protocol::ReviewDecision;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxType;

pub(crate) struct ToolOrchestrator {
    sandbox: SandboxManager,
}

pub(crate) struct OrchestratorRunResult<Out> {
    pub output: Out,
    pub deferred_network_approval: Option<DeferredNetworkApproval>,
}

impl ToolOrchestrator {
    pub fn new() -> Self {
        Self {
            sandbox: SandboxManager::new(),
        }
    }

    async fn run_attempt<Rq, Out, T>(
        tool: &mut T,
        req: &Rq,
        tool_ctx: &ToolCtx,
        attempt: &SandboxAttempt<'_>,
        managed_network_active: bool,
    ) -> (Result<Out, ToolError>, Option<DeferredNetworkApproval>)
    where
        T: ToolRuntime<Rq, Out>,
    {
        let network_approval = begin_network_approval(
            &tool_ctx.session,
            &tool_ctx.turn.sub_id,
            managed_network_active,
            tool.network_approval_spec(req, tool_ctx),
        )
        .await;

        let attempt_tool_ctx = ToolCtx {
            session: tool_ctx.session.clone(),
            turn: tool_ctx.turn.clone(),
            call_id: tool_ctx.call_id.clone(),
            tool_name: tool_ctx.tool_name.clone(),
        };
        let run_result = tool.run(req, attempt, &attempt_tool_ctx).await;

        let Some(network_approval) = network_approval else {
            return (run_result, None);
        };

        match network_approval.mode() {
            NetworkApprovalMode::Immediate => {
                let finalize_result =
                    finish_immediate_network_approval(&tool_ctx.session, network_approval).await;
                if let Err(err) = finalize_result {
                    return (Err(err), None);
                }
                (run_result, None)
            }
            NetworkApprovalMode::Deferred => {
                let deferred = network_approval.into_deferred();
                if run_result.is_err() {
                    finish_deferred_network_approval(&tool_ctx.session, deferred).await;
                    return (run_result, None);
                }
                (run_result, deferred)
            }
        }
    }

    pub async fn run<Rq, Out, T>(
        &mut self,
        tool: &mut T,
        req: &Rq,
        tool_ctx: &ToolCtx,
        turn_ctx: &crate::session::turn_context::TurnContext,
        approval_policy: AskForApproval,
    ) -> Result<OrchestratorRunResult<Out>, ToolError>
    where
        T: ToolRuntime<Rq, Out>,
    {
        let otel = turn_ctx.session_telemetry.clone();
        let otel_tn = &tool_ctx.tool_name;
        let otel_ci = &tool_ctx.call_id;
        let use_guardian = routes_approval_to_guardian(turn_ctx);

        // 1) Approval
        let mut already_approved = false;

        let requirement = tool.exec_approval_requirement(req).unwrap_or_else(|| {
            default_exec_approval_requirement(approval_policy, &turn_ctx.file_system_sandbox_policy)
        });
        tracing::debug!(
            call_id = %tool_ctx.call_id,
            tool_name = %tool_ctx.tool_name,
            ?approval_policy,
            file_system_sandbox_policy = ?turn_ctx.file_system_sandbox_policy,
            sandbox_policy = ?turn_ctx.sandbox_policy.get(),
            ?requirement,
            use_guardian,
            "tool orchestrator resolved initial approval requirement"
        );
        match requirement {
            ExecApprovalRequirement::Skip { .. } => {
                otel.tool_decision(
                    otel_tn,
                    otel_ci,
                    &ReviewDecision::Approved,
                    ToolDecisionSource::Config,
                );
            }
            ExecApprovalRequirement::Forbidden { reason } => {
                tracing::debug!(
                    call_id = %tool_ctx.call_id,
                    tool_name = %tool_ctx.tool_name,
                    reason = %reason,
                    "tool orchestrator rejected before first attempt because approval requirement was forbidden"
                );
                return Err(ToolError::Rejected(reason));
            }
            ExecApprovalRequirement::NeedsApproval { reason, .. } => {
                let guardian_review_id = use_guardian.then(new_guardian_review_id);
                let approval_ctx = ApprovalCtx {
                    session: &tool_ctx.session,
                    turn: &tool_ctx.turn,
                    call_id: &tool_ctx.call_id,
                    guardian_review_id: guardian_review_id.clone(),
                    retry_reason: reason,
                    network_approval_context: None,
                };
                let decision = Self::request_approval(
                    tool,
                    req,
                    tool_ctx.call_id.as_str(),
                    approval_ctx,
                    tool_ctx,
                    use_guardian,
                    &otel,
                )
                .await?;
                tracing::debug!(
                    call_id = %tool_ctx.call_id,
                    tool_name = %tool_ctx.tool_name,
                    ?decision,
                    "tool orchestrator received initial approval decision"
                );

                match decision {
                    ReviewDecision::Denied | ReviewDecision::Abort => {
                        let reason = if let Some(review_id) = guardian_review_id.as_deref() {
                            guardian_rejection_message(tool_ctx.session.as_ref(), review_id).await
                        } else {
                            "rejected by user".to_string()
                        };
                        tracing::debug!(
                            call_id = %tool_ctx.call_id,
                            tool_name = %tool_ctx.tool_name,
                            ?decision,
                            reason = %reason,
                            "tool orchestrator initial approval denied"
                        );
                        return Err(ToolError::Rejected(reason));
                    }
                    ReviewDecision::TimedOut => {
                        tracing::debug!(
                            call_id = %tool_ctx.call_id,
                            tool_name = %tool_ctx.tool_name,
                            "tool orchestrator initial approval timed out"
                        );
                        return Err(ToolError::Rejected(guardian_timeout_message()));
                    }
                    ReviewDecision::Approved
                    | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                    | ReviewDecision::ApprovedForSession => {}
                    ReviewDecision::NetworkPolicyAmendment {
                        network_policy_amendment,
                    } => match network_policy_amendment.action {
                        NetworkPolicyRuleAction::Allow => {}
                        NetworkPolicyRuleAction::Deny => {
                            tracing::debug!(
                                call_id = %tool_ctx.call_id,
                                tool_name = %tool_ctx.tool_name,
                                "tool orchestrator initial network policy amendment denied"
                            );
                            return Err(ToolError::Rejected("rejected by user".to_string()));
                        }
                    },
                }
                already_approved = true;
            }
        }

        // 2) First attempt under the selected sandbox.
        let managed_network_active = turn_ctx.network.is_some();
        let initial_sandbox = match tool.sandbox_mode_for_first_attempt(req) {
            SandboxOverride::BypassSandboxFirstAttempt => SandboxType::None,
            SandboxOverride::NoOverride => self.sandbox.select_initial(
                &turn_ctx.file_system_sandbox_policy,
                turn_ctx.network_sandbox_policy,
                tool.sandbox_preference(),
                turn_ctx.windows_sandbox_level,
                managed_network_active,
            ),
        };
        tracing::debug!(
            call_id = %tool_ctx.call_id,
            tool_name = %tool_ctx.tool_name,
            ?initial_sandbox,
            managed_network_active,
            sandbox_preference = ?tool.sandbox_preference(),
            network_sandbox_policy = ?turn_ctx.network_sandbox_policy,
            windows_sandbox_level = ?turn_ctx.windows_sandbox_level,
            "tool orchestrator selected initial sandbox"
        );

        // Platform-specific flag gating is handled by SandboxManager::select_initial.
        let use_legacy_landlock = turn_ctx.features.use_legacy_landlock();
        let initial_attempt = SandboxAttempt {
            sandbox: initial_sandbox,
            policy: &turn_ctx.sandbox_policy,
            file_system_policy: &turn_ctx.file_system_sandbox_policy,
            network_policy: turn_ctx.network_sandbox_policy,
            enforce_managed_network: managed_network_active,
            manager: &self.sandbox,
            sandbox_cwd: &turn_ctx.cwd,
            codex_linux_sandbox_exe: turn_ctx.codex_linux_sandbox_exe.as_ref(),
            use_legacy_landlock,
            windows_sandbox_level: turn_ctx.windows_sandbox_level,
            windows_sandbox_private_desktop: turn_ctx
                .config
                .permissions
                .windows_sandbox_private_desktop,
        };

        let (first_result, first_deferred_network_approval) = Self::run_attempt(
            tool,
            req,
            tool_ctx,
            &initial_attempt,
            managed_network_active,
        )
        .await;
        match first_result {
            Ok(out) => {
                // We have a successful initial result
                tracing::debug!(
                    call_id = %tool_ctx.call_id,
                    tool_name = %tool_ctx.tool_name,
                    ?initial_sandbox,
                    "tool orchestrator first attempt succeeded"
                );
                Ok(OrchestratorRunResult {
                    output: out,
                    deferred_network_approval: first_deferred_network_approval,
                })
            }
            Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                output,
                network_policy_decision,
            }))) => {
                let network_approval_context = if managed_network_active {
                    network_policy_decision
                        .as_ref()
                        .and_then(network_approval_context_from_payload)
                } else {
                    None
                };
                tracing::debug!(
                    call_id = %tool_ctx.call_id,
                    tool_name = %tool_ctx.tool_name,
                    ?initial_sandbox,
                    exit_code = output.exit_code,
                    stdout = %output.stdout.text,
                    stderr = %output.stderr.text,
                    aggregated_output = %output.aggregated_output.text,
                    ?network_policy_decision,
                    ?network_approval_context,
                    "tool orchestrator first attempt failed with sandbox denial"
                );
                if network_policy_decision.is_some() && network_approval_context.is_none() {
                    tracing::debug!(
                        call_id = %tool_ctx.call_id,
                        tool_name = %tool_ctx.tool_name,
                        "tool orchestrator returning sandbox denial because network decision had no approval context"
                    );
                    return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output,
                        network_policy_decision,
                    })));
                }
                if !tool.escalate_on_failure() {
                    tracing::debug!(
                        call_id = %tool_ctx.call_id,
                        tool_name = %tool_ctx.tool_name,
                        "tool orchestrator returning sandbox denial because tool does not escalate on failure"
                    );
                    return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                        output,
                        network_policy_decision,
                    })));
                }
                // Under `Never` or `OnRequest`, do not retry without sandbox;
                // surface a concise sandbox denial that preserves the
                // original output.
                if !tool.wants_no_sandbox_approval(approval_policy) {
                    let allow_on_request_network_prompt =
                        matches!(approval_policy, AskForApproval::OnRequest)
                            && network_approval_context.is_some()
                            && matches!(
                                default_exec_approval_requirement(
                                    approval_policy,
                                    &turn_ctx.file_system_sandbox_policy
                                ),
                                ExecApprovalRequirement::NeedsApproval { .. }
                            );
                    if !allow_on_request_network_prompt {
                        tracing::debug!(
                            call_id = %tool_ctx.call_id,
                            tool_name = %tool_ctx.tool_name,
                            ?approval_policy,
                            ?network_approval_context,
                            "tool orchestrator returning sandbox denial because no-sandbox approval prompt is not allowed"
                        );
                        return Err(ToolError::Codex(CodexErr::Sandbox(SandboxErr::Denied {
                            output,
                            network_policy_decision,
                        })));
                    }
                }
                let retry_reason =
                    if let Some(network_approval_context) = network_approval_context.as_ref() {
                        format!(
                            "Network access to \"{}\" is blocked by policy.",
                            network_approval_context.host
                        )
                    } else {
                        tracing::debug!(
                            call_id = %tool_ctx.call_id,
                            tool_name = %tool_ctx.tool_name,
                            exit_code = output.exit_code,
                            stdout = %output.stdout.text,
                            stderr = %output.stderr.text,
                            aggregated_output = %output.aggregated_output.text,
                            "sandbox denial classified; preparing no-sandbox retry approval"
                        );
                        build_denial_reason_from_output(output.as_ref())
                    };

                // Ask for approval before retrying with the escalated sandbox.
                let bypass_retry_approval = tool
                    .should_bypass_approval(approval_policy, already_approved)
                    && network_approval_context.is_none();
                tracing::debug!(
                    call_id = %tool_ctx.call_id,
                    tool_name = %tool_ctx.tool_name,
                    bypass_retry_approval,
                    already_approved,
                    ?approval_policy,
                    ?network_approval_context,
                    "tool orchestrator evaluated retry approval requirement"
                );
                if !bypass_retry_approval {
                    let guardian_review_id = use_guardian.then(new_guardian_review_id);
                    let approval_ctx = ApprovalCtx {
                        session: &tool_ctx.session,
                        turn: &tool_ctx.turn,
                        call_id: &tool_ctx.call_id,
                        guardian_review_id: guardian_review_id.clone(),
                        retry_reason: Some(retry_reason),
                        network_approval_context: network_approval_context.clone(),
                    };

                    let permission_request_run_id = format!("{}:retry", tool_ctx.call_id);
                    let decision = Self::request_approval(
                        tool,
                        req,
                        &permission_request_run_id,
                        approval_ctx,
                        tool_ctx,
                        use_guardian,
                        &otel,
                    )
                    .await?;
                    tracing::debug!(
                        call_id = %tool_ctx.call_id,
                        tool_name = %tool_ctx.tool_name,
                        ?decision,
                        "tool orchestrator received retry approval decision"
                    );

                    match decision {
                        ReviewDecision::Denied | ReviewDecision::Abort => {
                            let reason = if let Some(review_id) = guardian_review_id.as_deref() {
                                guardian_rejection_message(tool_ctx.session.as_ref(), review_id)
                                    .await
                            } else {
                                "rejected by user".to_string()
                            };
                            tracing::debug!(
                                call_id = %tool_ctx.call_id,
                                tool_name = %tool_ctx.tool_name,
                                ?decision,
                                reason = %reason,
                                "tool orchestrator retry approval denied"
                            );
                            return Err(ToolError::Rejected(reason));
                        }
                        ReviewDecision::TimedOut => {
                            tracing::debug!(
                                call_id = %tool_ctx.call_id,
                                tool_name = %tool_ctx.tool_name,
                                "tool orchestrator retry approval timed out"
                            );
                            return Err(ToolError::Rejected(guardian_timeout_message()));
                        }
                        ReviewDecision::Approved
                        | ReviewDecision::ApprovedExecpolicyAmendment { .. }
                        | ReviewDecision::ApprovedForSession => {}
                        ReviewDecision::NetworkPolicyAmendment {
                            network_policy_amendment,
                        } => match network_policy_amendment.action {
                            NetworkPolicyRuleAction::Allow => {}
                            NetworkPolicyRuleAction::Deny => {
                                tracing::debug!(
                                    call_id = %tool_ctx.call_id,
                                    tool_name = %tool_ctx.tool_name,
                                    "tool orchestrator retry network policy amendment denied"
                                );
                                return Err(ToolError::Rejected("rejected by user".to_string()));
                            }
                        },
                    }
                }

                let escalated_attempt = SandboxAttempt {
                    sandbox: SandboxType::None,
                    policy: &turn_ctx.sandbox_policy,
                    file_system_policy: &turn_ctx.file_system_sandbox_policy,
                    network_policy: turn_ctx.network_sandbox_policy,
                    enforce_managed_network: managed_network_active,
                    manager: &self.sandbox,
                    sandbox_cwd: &turn_ctx.cwd,
                    codex_linux_sandbox_exe: None,
                    use_legacy_landlock,
                    windows_sandbox_level: turn_ctx.windows_sandbox_level,
                    windows_sandbox_private_desktop: turn_ctx
                        .config
                        .permissions
                        .windows_sandbox_private_desktop,
                };

                // Second attempt.
                let (retry_result, retry_deferred_network_approval) = Self::run_attempt(
                    tool,
                    req,
                    tool_ctx,
                    &escalated_attempt,
                    managed_network_active,
                )
                .await;
                match retry_result {
                    Ok(output) => {
                        tracing::debug!(
                            call_id = %tool_ctx.call_id,
                            tool_name = %tool_ctx.tool_name,
                            "tool orchestrator no-sandbox retry succeeded"
                        );
                        Ok(OrchestratorRunResult {
                            output,
                            deferred_network_approval: retry_deferred_network_approval,
                        })
                    }
                    Err(err) => {
                        tracing::debug!(
                            call_id = %tool_ctx.call_id,
                            tool_name = %tool_ctx.tool_name,
                            ?err,
                            "tool orchestrator no-sandbox retry failed"
                        );
                        Err(err)
                    }
                }
            }
            Err(err) => {
                tracing::debug!(
                    call_id = %tool_ctx.call_id,
                    tool_name = %tool_ctx.tool_name,
                    ?err,
                    "tool orchestrator first attempt failed with non-sandbox-denial error"
                );
                Err(err)
            }
        }
    }

    // PermissionRequest hooks take top precedence for answering approval
    // prompts. If no matching hook returns a decision, fall back to the
    // normal guardian or user approval path.
    async fn request_approval<Rq, Out, T>(
        tool: &mut T,
        req: &Rq,
        permission_request_run_id: &str,
        approval_ctx: ApprovalCtx<'_>,
        tool_ctx: &ToolCtx,
        use_guardian: bool,
        otel: &codex_otel::SessionTelemetry,
    ) -> Result<ReviewDecision, ToolError>
    where
        T: ToolRuntime<Rq, Out>,
    {
        if let Some(permission_request) = tool.permission_request_payload(req) {
            match run_permission_request_hooks(
                approval_ctx.session,
                approval_ctx.turn,
                permission_request_run_id,
                permission_request,
            )
            .await
            {
                Some(PermissionRequestDecision::Allow) => {
                    let decision = ReviewDecision::Approved;
                    otel.tool_decision(
                        &tool_ctx.tool_name,
                        &tool_ctx.call_id,
                        &decision,
                        ToolDecisionSource::Config,
                    );
                    return Ok(decision);
                }
                Some(PermissionRequestDecision::Deny { message }) => {
                    let decision = ReviewDecision::Denied;
                    otel.tool_decision(
                        &tool_ctx.tool_name,
                        &tool_ctx.call_id,
                        &decision,
                        ToolDecisionSource::Config,
                    );
                    return Err(ToolError::Rejected(message));
                }
                None => {}
            }
        }

        let decision = tool.start_approval_async(req, approval_ctx).await;
        let otel_source = if use_guardian {
            ToolDecisionSource::AutomatedReviewer
        } else {
            ToolDecisionSource::User
        };
        otel.tool_decision(
            &tool_ctx.tool_name,
            &tool_ctx.call_id,
            &decision,
            otel_source,
        );
        Ok(decision)
    }
}

fn build_denial_reason_from_output(_output: &ExecToolCallOutput) -> String {
    // Keep approval reason terse and stable for UX/tests, but accept the
    // output so we can evolve heuristics later without touching call sites.
    "command failed; retry without sandbox?".to_string()
}
