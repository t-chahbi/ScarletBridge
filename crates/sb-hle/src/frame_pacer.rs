//! Machine à états de cadencement console Xbox Series X (`SetFrameIntervalX`,
//! `ScheduleFrameEventX`, `WaitFrameEventX`, `GetFrameStatisticsX`, `PresentX`,
//! `SuspendX` / `ResumeX`).
//!
//! Suit le contrat observé sur les échantillons GDKX publics :
//! - jetons 64 bits strictement positifs et monotones (`0` = `FRAME_PIPELINE_TOKEN_NULL`) ;
//! - suivi des origines programmées, des images en vol (`latency`), des statistiques
//!   `origin` / `present` / `display` ;
//! - calcul `ImmediateThresholdPercent` pour décider d'un basculement immédiat vs VBlank ;
//! - suspend/resume : les événements doivent être réenregistrés après `ResumeX`.

#![allow(dead_code)]

use std::collections::VecDeque;

use sb_common::abi::{E_INVALIDARG, HResult, S_OK};

/// `HRESULT_FROM_WIN32(ERROR_INVALID_STATE)` (`0x8007139F`).
pub const HRESULT_INVALID_STATE: HResult = 0x8007_139Fu32 as i32;

pub const FRAME_EVENT_ORIGIN: u32 = 0;

pub const STAT_FRAME_INTERVAL: u32 = 0x01;
pub const STAT_FRAME_EVENT: u32 = 0x02;
pub const STAT_RENDER: u32 = 0x04;
pub const STAT_PRESENT: u32 = 0x08;
pub const STAT_DISPLAY: u32 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentSync {
    VBlank,
    Immediate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenIssue {
    None,
    NullToken,
    Stale,
    Repeated,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameStatRecord {
    pub stat_type: u32,
    pub plane_index: u32,
    pub token: u64,
    pub origin_time_us: u64,
    pub flip_time_us: u64,
    pub percent_scanned: f32,
}

#[derive(Debug, Clone, Copy)]
struct TokenEntry {
    token: u64,
    origin_us: u64,
    due_vblank_us: u64,
    presented_us: Option<u64>,
    displayed_us: Option<u64>,
    percent_scanned: f32,
}

/// Décide si une image en retard de `late_by_us` peut basculer immédiatement (déchirure contrôlée)
/// selon `ImmediateThresholdPercent`.
pub fn decide_present_sync(
    late_by_us: u64,
    interval_us: u32,
    threshold_percent: f32,
) -> PresentSync {
    if !(threshold_percent > 0.0) || late_by_us == 0 || interval_us == 0 {
        return PresentSync::VBlank;
    }
    let scanned = 100.0 * ((late_by_us % u64::from(interval_us)) as f64) / f64::from(interval_us);
    if scanned <= f64::from(threshold_percent.min(100.0)) {
        PresentSync::Immediate
    } else {
        PresentSync::VBlank
    }
}

pub fn scan_percent(late_by_us: u64, interval_us: u32) -> f32 {
    if interval_us == 0 {
        0.0
    } else {
        (100.0 * ((late_by_us % u64::from(interval_us)) as f64) / f64::from(interval_us)) as f32
    }
}

pub struct FramePacer {
    period_us: u32,
    latency: u32,
    offset_us: u32,
    epoch_us: u64,
    interval_set: bool,
    armed: bool,
    suspended: bool,
    next_token: u64,
    last_presented_token: u64,
    present_count: u64,
    history: VecDeque<TokenEntry>,
}

impl Default for FramePacer {
    fn default() -> Self {
        Self::new()
    }
}

impl FramePacer {
    pub const fn new() -> Self {
        Self {
            period_us: 16_667,
            latency: 1,
            offset_us: 0,
            epoch_us: 0,
            interval_set: false,
            armed: false,
            suspended: false,
            next_token: 1,
            last_presented_token: 0,
            present_count: 0,
            history: VecDeque::new(),
        }
    }

    pub const fn period_us(&self) -> u32 {
        self.period_us
    }

    pub const fn is_registered(&self) -> bool {
        self.interval_set && self.armed && !self.suspended
    }

    pub const fn is_suspended(&self) -> bool {
        self.suspended
    }

    pub const fn present_count(&self) -> u64 {
        self.present_count
    }

    pub fn set_frame_interval(
        &mut self,
        now_us: u64,
        period_us: u32,
        latency: u32,
        _flags: u32,
    ) -> HResult {
        if period_us == 0 {
            return E_INVALIDARG;
        }
        if self.suspended {
            return HRESULT_INVALID_STATE;
        }
        self.period_us = period_us;
        self.latency = latency.max(1);
        self.epoch_us = now_us;
        self.interval_set = true;
        self.offset_us = self.offset_us.min(period_us - 1);
        S_OK
    }

    pub fn schedule_frame_event(
        &mut self,
        _now_us: u64,
        event: u32,
        offset_us: u32,
        _flags: u32,
    ) -> HResult {
        if self.suspended {
            return HRESULT_INVALID_STATE;
        }
        // Sur console, le titre peut planifier l'événement avant ou après `SetFrameIntervalX`.
        if event == FRAME_EVENT_ORIGIN {
            self.offset_us = offset_us.min(self.period_us.saturating_sub(1));
            self.armed = true;
        }
        S_OK
    }

    /// Alloue un jeton monotone non nul et enregistre son horodatage d'origine.
    pub fn wait_frame_event(&mut self, now_us: u64, _event: u32) -> Result<u64, HResult> {
        if self.suspended {
            return Err(HRESULT_INVALID_STATE);
        }
        let mut token = self.next_token & 0x0000_FFFF_FFFF_FFFF;
        if token == 0 {
            token = 1;
        }
        self.next_token = token.wrapping_add(1);
        let due = now_us.saturating_add(u64::from(self.period_us));
        self.history.push_back(TokenEntry {
            token,
            origin_us: now_us,
            due_vblank_us: due,
            presented_us: None,
            displayed_us: None,
            percent_scanned: 0.0,
        });
        while self.history.len() > 64 {
            self.history.pop_front();
        }
        Ok(token)
    }

    /// Vérifie un jeton reçu par `PresentX` et classe d'éventuelles anomalies sans bloquer.
    pub fn check_present_token(&self, token: u64) -> TokenIssue {
        if token == 0 {
            return TokenIssue::NullToken;
        }
        if token == self.last_presented_token {
            return TokenIssue::Repeated;
        }
        if token < self.last_presented_token {
            return TokenIssue::Stale;
        }
        TokenIssue::None
    }

    /// Enregistre une présentation effectuée (`PresentX`) et renvoie le numéro d'image (1-based).
    pub fn commit_present(
        &mut self,
        now_us: u64,
        token: u64,
        threshold_percent: f32,
    ) -> (u64, PresentSync) {
        self.present_count = self.present_count.saturating_add(1);
        if token != 0 && token > self.last_presented_token {
            self.last_presented_token = token;
        }
        let mut sync = PresentSync::VBlank;
        if let Some(entry) = self.history.iter_mut().rev().find(|e| e.token == token) {
            let late_us = now_us.saturating_sub(entry.due_vblank_us);
            sync = decide_present_sync(late_us, self.period_us, threshold_percent);
            entry.presented_us = Some(now_us);
            entry.displayed_us = Some(if sync == PresentSync::Immediate {
                now_us
            } else {
                now_us.max(entry.due_vblank_us)
            });
            entry.percent_scanned = if sync == PresentSync::Immediate {
                scan_percent(late_us, self.period_us)
            } else {
                0.0
            };
        }
        (self.present_count, sync)
    }

    pub fn suspend(&mut self) {
        self.suspended = true;
        self.armed = false;
    }

    pub fn resume(&mut self) {
        self.suspended = false;
    }

    /// Collecte les statistiques d'un jeton (ou de la dernière image si `token == 0`).
    pub fn get_statistics(&self, now_us: u64, token: u64, type_mask: u32) -> Vec<FrameStatRecord> {
        let mask = if type_mask == 0 {
            STAT_FRAME_INTERVAL | STAT_FRAME_EVENT | STAT_PRESENT | STAT_DISPLAY
        } else {
            type_mask
        };
        let entry = if token != 0 {
            self.history
                .iter()
                .rev()
                .find(|e| e.token == token)
                .copied()
        } else {
            self.history.back().copied()
        };
        let (tok, origin_us, flip_us, pct) = match entry {
            Some(e) => (
                e.token,
                e.origin_us,
                e.displayed_us.or(e.presented_us).unwrap_or(e.due_vblank_us),
                e.percent_scanned,
            ),
            None => (
                token,
                now_us,
                now_us.saturating_add(u64::from(self.period_us)),
                0.0,
            ),
        };
        let mut out = Vec::new();
        for bit in [
            STAT_FRAME_INTERVAL,
            STAT_FRAME_EVENT,
            STAT_PRESENT,
            STAT_DISPLAY,
        ] {
            if mask & bit != 0 {
                out.push(FrameStatRecord {
                    stat_type: bit,
                    plane_index: 0,
                    token: tok,
                    origin_time_us: origin_us,
                    flip_time_us: flip_us,
                    percent_scanned: pct,
                });
            }
        }
        if out.is_empty() {
            out.push(FrameStatRecord {
                stat_type: 1u32 << mask.trailing_zeros(),
                plane_index: 0,
                token: tok,
                origin_time_us: origin_us,
                flip_time_us: flip_us,
                percent_scanned: pct,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacer_lifecycle_tokens_stats_and_tear_threshold() {
        let mut p = FramePacer::new();
        assert_eq!(p.set_frame_interval(1_000, 16_667, 2, 0), S_OK);
        assert_eq!(
            p.schedule_frame_event(1_000, FRAME_EVENT_ORIGIN, 500, 0),
            S_OK
        );
        assert!(p.is_registered());

        let t1 = p.wait_frame_event(2_000, FRAME_EVENT_ORIGIN).expect("t1");
        let t2 = p.wait_frame_event(18_667, FRAME_EVENT_ORIGIN).expect("t2");
        assert_eq!((t1, t2), (1, 2));
        assert_eq!(p.check_present_token(t1), TokenIssue::None);
        assert_eq!(p.check_present_token(0), TokenIssue::NullToken);

        let (frame1, sync1) = p.commit_present(19_000, t1, 50.0);
        assert_eq!(frame1, 1);
        // `t1` due à 18_667 -> en retard de 333 us (~2 % de 16_667) <= 50 % -> Immediate.
        assert_eq!(sync1, PresentSync::Immediate);
        assert_eq!(p.check_present_token(t1), TokenIssue::Repeated);

        let stats = p.get_statistics(20_000, t1, STAT_PRESENT | STAT_DISPLAY);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].token, t1);
        assert!(stats[1].percent_scanned > 1.0 && stats[1].percent_scanned < 3.0);

        p.suspend();
        assert!(p.is_suspended());
        assert_eq!(p.wait_frame_event(21_000, 0), Err(HRESULT_INVALID_STATE));
        p.resume();
        assert!(
            !p.is_registered(),
            "l'événement doit être réarmé après ResumeX"
        );
    }
}
