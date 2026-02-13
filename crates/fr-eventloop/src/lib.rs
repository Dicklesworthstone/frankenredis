#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickBudget {
    pub max_accepts: usize,
    pub max_commands: usize,
}

impl Default for TickBudget {
    fn default() -> Self {
        Self {
            max_accepts: 64,
            max_commands: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickStats {
    pub accepted: usize,
    pub processed_commands: usize,
    pub accept_backlog_remaining: usize,
    pub command_backlog_remaining: usize,
}

#[must_use]
pub fn run_tick(pending_accepts: usize, pending_commands: usize, budget: TickBudget) -> TickStats {
    let accepted = pending_accepts.min(budget.max_accepts);
    let processed_commands = pending_commands.min(budget.max_commands);
    TickStats {
        accepted,
        processed_commands,
        accept_backlog_remaining: pending_accepts.saturating_sub(accepted),
        command_backlog_remaining: pending_commands.saturating_sub(processed_commands),
    }
}

#[cfg(test)]
mod tests {
    use super::{TickBudget, run_tick};

    #[test]
    fn tick_respects_budget() {
        let stats = run_tick(
            100,
            10_000,
            TickBudget {
                max_accepts: 10,
                max_commands: 500,
            },
        );
        assert_eq!(stats.accepted, 10);
        assert_eq!(stats.processed_commands, 500);
        assert_eq!(stats.accept_backlog_remaining, 90);
        assert_eq!(stats.command_backlog_remaining, 9_500);
    }
}
