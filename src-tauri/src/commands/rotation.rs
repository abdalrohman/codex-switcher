//! Experimental auto-rotation commands

use crate::api::usage::get_account_usage;
use crate::auth::{get_active_account, load_accounts, load_settings};
use crate::commands::account::switch_account;
use crate::commands::process::check_active_codex_process_count;
use crate::types::{AuthMode, StoredAccount, UsageInfo};

#[derive(Debug, serde::Serialize)]
pub struct AutoRotateResult {
    pub enabled: bool,
    pub triggered: bool,
    pub rotated: bool,
    pub reason: String,
    pub from_account_id: Option<String>,
    pub from_account_name: Option<String>,
    pub to_account_id: Option<String>,
    pub to_account_name: Option<String>,
    pub opencode_synced: bool,
    pub openclaw_synced: bool,
}

#[derive(Debug)]
struct CandidateScore {
    account: StoredAccount,
    score: f64,
}

#[tauri::command]
pub async fn evaluate_auto_rotate() -> Result<AutoRotateResult, String> {
    let settings = load_settings().unwrap_or_default();
    if !settings.experimental_auto_rotate_enabled {
        return Ok(AutoRotateResult {
            enabled: false,
            triggered: false,
            rotated: false,
            reason: "Experimental auto-rotate is disabled".to_string(),
            from_account_id: None,
            from_account_name: None,
            to_account_id: None,
            to_account_name: None,
            opencode_synced: false,
            openclaw_synced: false,
        });
    }

    let active = match get_active_account().map_err(|e| e.to_string())? {
        Some(account) => account,
        None => {
            return Ok(AutoRotateResult {
                enabled: true,
                triggered: false,
                rotated: false,
                reason: "No active account is selected".to_string(),
                from_account_id: None,
                from_account_name: None,
                to_account_id: None,
                to_account_name: None,
                opencode_synced: false,
                openclaw_synced: false,
            });
        }
    };

    let active_usage = match get_account_usage(&active).await {
        Ok(usage) => usage,
        Err(err) => {
            return Ok(AutoRotateResult {
                enabled: true,
                triggered: false,
                rotated: false,
                reason: format!("Could not evaluate active account health: {err}"),
                from_account_id: Some(active.id.clone()),
                from_account_name: Some(active.name.clone()),
                to_account_id: None,
                to_account_name: None,
                opencode_synced: false,
                openclaw_synced: false,
            });
        }
    };
    let trigger_reason = match rotation_trigger_reason(&active, &active_usage) {
        Some(reason) => reason,
        None => {
            return Ok(AutoRotateResult {
                enabled: true,
                triggered: false,
                rotated: false,
                reason: "Active account still looks healthy".to_string(),
                from_account_id: Some(active.id.clone()),
                from_account_name: Some(active.name.clone()),
                to_account_id: None,
                to_account_name: None,
                opencode_synced: false,
                openclaw_synced: false,
            });
        }
    };

    let active_process_count = check_active_codex_process_count().map_err(|e| e.to_string())?;
    if active_process_count > 0 {
        return Ok(AutoRotateResult {
            enabled: true,
            triggered: true,
            rotated: false,
            reason: format!(
                "Active account is exhausted, but {active_process_count} Codex process(es) are still running"
            ),
            from_account_id: Some(active.id.clone()),
            from_account_name: Some(active.name.clone()),
            to_account_id: None,
            to_account_name: None,
            opencode_synced: false,
            openclaw_synced: false,
        });
    }

    let store = load_accounts().map_err(|e| e.to_string())?;
    let mut best_candidate: Option<CandidateScore> = None;

    for candidate in store
        .accounts
        .into_iter()
        .filter(|account| account.id != active.id)
    {
        if let Some(score) = score_reserve_account(&candidate).await {
            let replace = best_candidate
                .as_ref()
                .map(|current| score.score > current.score)
                .unwrap_or(true);
            if replace {
                best_candidate = Some(score);
            }
        }
    }

    let Some(best_candidate) = best_candidate else {
        return Ok(AutoRotateResult {
            enabled: true,
            triggered: true,
            rotated: false,
            reason: format!("{trigger_reason}, but no healthy reserve account is available"),
            from_account_id: Some(active.id.clone()),
            from_account_name: Some(active.name.clone()),
            to_account_id: None,
            to_account_name: None,
            opencode_synced: false,
            openclaw_synced: false,
        });
    };

    let switched = switch_account(best_candidate.account.id.clone()).await?;

    Ok(AutoRotateResult {
        enabled: true,
        triggered: true,
        rotated: true,
        reason: trigger_reason,
        from_account_id: Some(active.id),
        from_account_name: Some(active.name),
        to_account_id: Some(best_candidate.account.id),
        to_account_name: Some(best_candidate.account.name),
        opencode_synced: switched.opencode_synced,
        openclaw_synced: switched.openclaw_synced,
    })
}

fn rotation_trigger_reason(account: &StoredAccount, usage: &UsageInfo) -> Option<String> {
    if matches!(account.auth_mode, AuthMode::ApiKey) {
        return None;
    }

    if usage.primary_used_percent.is_some_and(|used| used >= 99.5) {
        return Some("active account hit the primary usage limit".to_string());
    }

    if usage
        .secondary_used_percent
        .is_some_and(|used| used >= 99.5)
    {
        return Some("active account hit the secondary usage limit".to_string());
    }

    if usage.has_credits == Some(false) && usage.unlimited_credits == Some(false) {
        return Some("active account has no credits remaining".to_string());
    }

    let error = usage.error.as_deref()?.to_ascii_lowercase();
    if error.contains("429") || error.contains("quota") || error.contains("limit") {
        return Some("active account returned a limit error".to_string());
    }
    if error.contains("401") || error.contains("403") || error.contains("unauthorized") {
        return Some("active account authorization is no longer usable".to_string());
    }

    None
}

async fn score_reserve_account(account: &StoredAccount) -> Option<CandidateScore> {
    match account.auth_mode {
        AuthMode::ApiKey => Some(CandidateScore {
            account: account.clone(),
            score: 35.0 + last_used_bonus(account),
        }),
        AuthMode::ChatGPT => {
            let usage = get_account_usage(account).await.ok()?;
            if usage.error.is_some() {
                return None;
            }
            if rotation_trigger_reason(account, &usage).is_some() {
                return None;
            }

            let remaining_primary = usage
                .primary_used_percent
                .map(|used| 100.0 - used)
                .unwrap_or(60.0);
            let remaining_secondary = usage
                .secondary_used_percent
                .map(|used| 100.0 - used)
                .unwrap_or(remaining_primary);
            let remaining_headroom = remaining_primary.min(remaining_secondary).max(0.0);

            let mut score = 50.0 + remaining_headroom;
            score += plan_bonus(account.plan_type.as_deref());
            score += if usage.unlimited_credits == Some(true) {
                12.0
            } else if usage.has_credits == Some(true) {
                6.0
            } else {
                0.0
            };
            score += last_used_bonus(account);

            Some(CandidateScore {
                account: account.clone(),
                score,
            })
        }
    }
}

fn plan_bonus(plan_type: Option<&str>) -> f64 {
    match plan_type.unwrap_or_default().to_ascii_lowercase().as_str() {
        "enterprise" => 18.0,
        "business" => 15.0,
        "team" => 12.0,
        "pro" => 10.0,
        "plus" => 6.0,
        "edu" => 5.0,
        "free" => 0.0,
        _ => 2.0,
    }
}

fn last_used_bonus(account: &StoredAccount) -> f64 {
    let Some(last_used_at) = account.last_used_at else {
        return 5.0;
    };

    let age_hours = (chrono::Utc::now() - last_used_at).num_hours().max(0) as f64;
    (age_hours / 24.0).min(5.0)
}
