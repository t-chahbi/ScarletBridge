//! Cycle de vie console (focus, suspend/resume, contraintes) et politique réseau offline.

use parking_lot::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {
    Running,
    Constrained,
    Suspended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkMode {
    /// Mode par défaut : toute télémétrie ou appel serveur console est bloqué localement.
    StrictOffline,
    /// Autorisé explicitement par l'utilisateur (`SB_OFFLINE_MODE=0`).
    OnlineAllowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleSnapshot {
    pub execution_state: ExecutionState,
    pub has_focus: bool,
    pub network_mode: NetworkMode,
}

pub struct LifecycleManager {
    state: RwLock<LifecycleSnapshot>,
}

impl Default for LifecycleManager {
    fn default() -> Self {
        Self::new(NetworkMode::StrictOffline)
    }
}

impl LifecycleManager {
    pub fn new(network_mode: NetworkMode) -> Self {
        Self {
            state: RwLock::new(LifecycleSnapshot {
                execution_state: ExecutionState::Running,
                has_focus: true,
                network_mode,
            }),
        }
    }

    pub fn from_env() -> Self {
        let offline = std::env::var("SB_OFFLINE_MODE")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        let mode = if offline {
            NetworkMode::StrictOffline
        } else {
            NetworkMode::OnlineAllowed
        };
        Self::new(mode)
    }

    pub fn snapshot(&self) -> LifecycleSnapshot {
        *self.state.read()
    }

    pub fn set_focus(&self, focused: bool) {
        let mut st = self.state.write();
        st.has_focus = focused;
        if st.execution_state != ExecutionState::Suspended {
            st.execution_state = if focused {
                ExecutionState::Running
            } else {
                ExecutionState::Constrained
            };
        }
    }

    pub fn suspend(&self) {
        let mut st = self.state.write();
        st.execution_state = ExecutionState::Suspended;
        st.has_focus = false;
    }

    pub fn resume(&self) {
        let mut st = self.state.write();
        st.execution_state = ExecutionState::Running;
        st.has_focus = true;
    }

    pub fn is_network_allowed(&self) -> bool {
        self.state.read().network_mode == NetworkMode::OnlineAllowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_focus_suspend_resume() {
        let mgr = LifecycleManager::default();
        assert!(!mgr.is_network_allowed());
        assert_eq!(mgr.snapshot().execution_state, ExecutionState::Running);

        mgr.set_focus(false);
        assert_eq!(mgr.snapshot().execution_state, ExecutionState::Constrained);

        mgr.suspend();
        assert_eq!(mgr.snapshot().execution_state, ExecutionState::Suspended);

        mgr.resume();
        assert_eq!(mgr.snapshot().execution_state, ExecutionState::Running);
        assert!(mgr.snapshot().has_focus);
    }
}
