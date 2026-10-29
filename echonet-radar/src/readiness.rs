//! Readiness answers one question, whether the LAN path carries datagrams. The
//! permission report is deliberately not an input: a firewall rule never proves
//! that a frame arrived.

use std::time::{Duration, SystemTime};

use echonet_radar_core::{Fault, RadarSnapshot, SessionStatus};

pub const STALE_AFTER_POLLS: u32 = 3;

pub const LOST_AFTER_POLLS: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    NotStarted,
    Binding,
    Unverified {
        since: SystemTime,
    },
    Listening {
        since: SystemTime,
        last: SystemTime,
        frames: u64,
    },
    Degraded {
        fault: Fault,
        last_frame: Option<SystemTime>,
    },
    Stopped,
}

impl Readiness {
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NotStarted => String::from("Not started"),
            Self::Binding => String::from("Binding 3610"),
            Self::Unverified { .. } => String::from("Bound, no datagram yet"),
            Self::Listening { frames, .. } => format!("Listening, {frames} frames"),
            Self::Degraded { fault, .. } => format!("Degraded, {}", describe_fault(fault)),
            Self::Stopped => String::from("Stopped"),
        }
    }

    #[must_use]
    pub const fn last_frame(&self) -> Option<SystemTime> {
        match self {
            Self::Listening { last, .. } => Some(*last),
            Self::Degraded { last_frame, .. } => *last_frame,
            Self::NotStarted | Self::Binding | Self::Unverified { .. } | Self::Stopped => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Lost,
}

#[must_use]
pub fn step(
    _state: &Readiness,
    snapshot: &RadarSnapshot,
) -> Readiness {
    match &snapshot.status {
        SessionStatus::Binding => Readiness::Binding,
        SessionStatus::Running { since } => {
            snapshot
                .last_frame
                .map_or(Readiness::Unverified { since: *since }, |last| {
                    Readiness::Listening {
                        since: *since,
                        last,
                        frames: snapshot.frames_seen,
                    }
                })
        },
        SessionStatus::Degraded { fault, last_frame } => Readiness::Degraded {
            fault: fault.clone(),
            last_frame: *last_frame,
        },
        SessionStatus::Stopped => Readiness::Stopped,
    }
}

/// A zero interval cannot happen through settings, which clamp at five seconds;
/// it is treated as lost at any positive age so the function stays total.
#[must_use]
pub fn freshness(
    last_seen: SystemTime,
    now: SystemTime,
    update_interval: Duration,
) -> Freshness {
    let elapsed = now.duration_since(last_seen).unwrap_or_default();
    if update_interval.is_zero() {
        return if elapsed.is_zero() {
            Freshness::Fresh
        } else {
            Freshness::Lost
        };
    }
    let missed = elapsed.as_nanos() / update_interval.as_nanos();
    if missed < u128::from(STALE_AFTER_POLLS) {
        Freshness::Fresh
    } else if missed < u128::from(LOST_AFTER_POLLS) {
        Freshness::Stale
    } else {
        Freshness::Lost
    }
}

#[must_use]
pub fn describe_fault(fault: &Fault) -> String {
    fault.to_string()
}
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;

    use echonet_radar_core::{ConfigError, SessionStatus};

    use super::*;

    fn snapshot(status: SessionStatus) -> RadarSnapshot {
        RadarSnapshot {
            revision: 1,
            session: 1,
            status,
            objects: BTreeMap::new(),
            recent: Vec::new(),
            notices: Vec::new(),
            frames_seen: 9,
            last_frame: None,
        }
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn every_status_row_maps_to_its_readiness() {
        let since = at(10);
        let last = at(20);
        let binding = snapshot(SessionStatus::Binding);
        assert_eq!(step(&Readiness::NotStarted, &binding), Readiness::Binding);

        let mut unverified = snapshot(SessionStatus::Running { since });
        unverified.frames_seen = 0;
        assert_eq!(
            step(&Readiness::Binding, &unverified),
            Readiness::Unverified { since }
        );

        let mut listening = snapshot(SessionStatus::Running { since });
        listening.last_frame = Some(last);
        assert_eq!(
            step(&Readiness::Unverified { since }, &listening),
            Readiness::Listening {
                since,
                last,
                frames: 9
            }
        );

        let fault = Fault::Bind {
            interface: Ipv4Addr::new(192, 168, 1, 2),
            detail: String::from("address in use"),
        };
        let degraded = snapshot(SessionStatus::Degraded {
            fault: fault.clone(),
            last_frame: Some(last),
        });
        assert_eq!(
            step(
                &Readiness::Listening {
                    since,
                    last,
                    frames: 9
                },
                &degraded
            ),
            Readiness::Degraded {
                fault,
                last_frame: Some(last)
            }
        );

        let stopped = snapshot(SessionStatus::Stopped);
        assert_eq!(
            step(
                &Readiness::Degraded {
                    fault: Fault::Config(ConfigError::ZeroInterval("update_interval")),
                    last_frame: None,
                },
                &stopped
            ),
            Readiness::Stopped
        );
    }

    #[test]
    fn step_is_idempotent_for_every_row() {
        let since = at(10);
        let cases = [
            Readiness::NotStarted,
            Readiness::Binding,
            Readiness::Unverified { since },
            Readiness::Listening {
                since,
                last: at(20),
                frames: 2,
            },
            Readiness::Degraded {
                fault: Fault::Service(String::from("socket closed")),
                last_frame: Some(at(20)),
            },
            Readiness::Stopped,
        ];
        let snapshots = [
            snapshot(SessionStatus::Binding),
            snapshot(SessionStatus::Running { since }),
            snapshot(SessionStatus::Stopped),
        ];
        for state in &cases {
            for snap in &snapshots {
                let once = step(state, snap);
                assert_eq!(step(&once, snap), once, "not idempotent for {state:?}");
            }
        }
    }

    #[test]
    fn freshness_holds_each_threshold() {
        let interval = Duration::from_secs(15);
        let now = at(1_000);
        let seen = |seconds_ago: u64| now - Duration::from_secs(seconds_ago);

        assert_eq!(freshness(seen(0), now, interval), Freshness::Fresh);
        assert_eq!(freshness(seen(44), now, interval), Freshness::Fresh);
        assert_eq!(freshness(seen(45), now, interval), Freshness::Stale);
        assert_eq!(freshness(seen(74), now, interval), Freshness::Stale);
        assert_eq!(freshness(seen(75), now, interval), Freshness::Lost);
        assert_eq!(freshness(seen(600), now, interval), Freshness::Lost);
    }

    #[test]
    fn freshness_treats_a_future_stamp_as_now() {
        let now = at(1_000);
        assert_eq!(
            freshness(now + Duration::from_secs(30), now, Duration::from_secs(15)),
            Freshness::Fresh
        );
    }

    #[test]
    fn freshness_with_a_zero_interval_is_total() {
        let now = at(1_000);
        assert_eq!(freshness(now, now, Duration::ZERO), Freshness::Fresh);
        assert_eq!(
            freshness(now - Duration::from_millis(1), now, Duration::ZERO),
            Freshness::Lost
        );
    }

    #[test]
    fn describe_names_each_state() {
        assert_eq!(Readiness::NotStarted.describe(), "Not started");
        assert!(Readiness::Binding.describe().contains("3610"));
        assert!(
            Readiness::Listening {
                since: at(1),
                last: at(2),
                frames: 7
            }
            .describe()
            .contains('7')
        );
    }

    #[test]
    fn last_frame_reads_the_listening_and_degraded_states() {
        let last = at(5);
        assert_eq!(
            Readiness::Listening {
                since: at(1),
                last,
                frames: 1
            }
            .last_frame(),
            Some(last)
        );
        assert_eq!(
            Readiness::Degraded {
                fault: Fault::Service(String::from("x")),
                last_frame: Some(last),
            }
            .last_frame(),
            Some(last)
        );
        assert_eq!(Readiness::Binding.last_frame(), None);
    }
}
