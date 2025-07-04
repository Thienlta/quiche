// Copyright (C) 2022, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! BBR Congestion Control
//!
//! This implementation is based on the following draft:
//! <https://tools.ietf.org/html/draft-cardwell-iccrg-bbr-congestion-control-00>

use super::*;
use crate::minmax::Minmax;

use std::time::Duration;

use super::CongestionControlOps;

pub(crate) static BBR: CongestionControlOps = CongestionControlOps {
    on_init,
    on_packet_sent,
    on_packets_acked,
    congestion_event,
    checkpoint,
    rollback,
    has_custom_pacing,
    #[cfg(feature = "qlog")]
    state_str,
    debug_fmt,
};

/// A constant specifying the length of the BBR.BtlBw max filter window for
/// BBR.BtlBwFilter, BtlBwFilterLen is 10 packet-timed round trips.
const BTLBW_FILTER_LEN: Duration = Duration::from_secs(10);

/// A constant specifying the minimum time interval between ProbeRTT states: 10
/// secs.
const PROBE_RTT_INTERVAL: Duration = Duration::from_secs(10);

/// A constant specifying the length of the RTProp min filter window.
const RTPROP_FILTER_LEN: Duration = PROBE_RTT_INTERVAL;

/// A constant specifying the minimum gain value that will allow the sending
/// rate to double each round (2/ln(2) ~= 2.89), used in Startup mode for both
/// BBR.pacing_gain and BBR.cwnd_gain.
const BBR_HIGH_GAIN: f64 = 2.89;

/// The minimal cwnd value BBR tries to target using: 4 packets, or 4 * SMSS
const BBR_MIN_PIPE_CWND_PKTS: usize = 4;

/// The number of phases in the BBR ProbeBW gain cycle: 8.
const BBR_GAIN_CYCLE_LEN: usize = 8;

/// A constant specifying the minimum duration for which ProbeRTT state holds
/// inflight to BBRMinPipeCwnd or fewer packets: 200 ms.
const PROBE_RTT_DURATION: Duration = Duration::from_millis(200);

/// Pacing Gain Cycle.
const PACING_GAIN_CYCLE: [f64; BBR_GAIN_CYCLE_LEN] =
    [5.0 / 4.0, 3.0 / 4.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];

/// A constant to check BBR.BtlBW is still growing.
const BTLBW_GROWTH_TARGET: f64 = 1.25;

/// BBR Internal State Machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BBRStateMachine {
    Startup,
    Drain,
    ProbeBW,
    ProbeRTT,
}

impl From<BBRStateMachine> for &'static str {
    fn from(state: BBRStateMachine) -> &'static str {
        match state {
            BBRStateMachine::Startup => "bbr_startup",
            BBRStateMachine::Drain => "bbr_drain",
            BBRStateMachine::ProbeBW => "bbr_probe_bw",
            BBRStateMachine::ProbeRTT => "bbr_probe_rtt",
        }
    }
}

/// BBR Specific State Variables.
pub struct State {
    // The current state of a BBR flow in the BBR state machine.
    state: BBRStateMachine,

    // The current pacing rate for a BBR flow, which controls inter-packet
    // spacing.
    pacing_rate: u64,

    // BBR's estimated bottleneck bandwidth available to the transport flow,
    // estimated from the maximum delivery rate sample in a sliding window.
    btlbw: u64,

    // The max filter used to estimate BBR.BtlBw.
    btlbwfilter: Minmax<u64>,

    // BBR's estimated two-way round-trip propagation delay of the path,
    // estimated from the windowed minimum recent round-trip delay sample.
    rtprop: Duration,

    // The wall clock time at which the current BBR.RTProp sample was obtained.
    rtprop_stamp: Instant,

    // A boolean recording whether the BBR.RTprop has expired and is due for a
    // refresh with an application idle period or a transition into ProbeRTT
    // state.
    rtprop_expired: bool,

    // The dynamic gain factor used to scale BBR.BtlBw to produce
    // BBR.pacing_rate.
    pacing_gain: f64,

    // The dynamic gain factor used to scale the estimated BDP to produce a
    // congestion window (cwnd).
    cwnd_gain: f64,

    // A boolean that records whether BBR estimates that it has ever fully
    // utilized its available bandwidth ("filled the pipe").
    filled_pipe: bool,

    // Count of packet-timed round trips elapsed so far.
    round_count: u64,

    // A boolean that BBR sets to true once per packet-timed round trip,
    // on ACKs that advance BBR.round_count.
    round_start: bool,

    // packet.delivered value denoting the end of a packet-timed round trip.
    next_round_delivered: usize,

    // Timestamp when ProbeRTT state ends.
    probe_rtt_done_stamp: Option<Instant>,

    // Checking if a roundtrip in ProbeRTT state ends.
    probe_rtt_round_done: bool,

    // Checking if in the packet conservation mode during recovery.
    packet_conservation: bool,

    // Saved cwnd before loss recovery.
    prior_cwnd: usize,

    // Checking if restarting from idle.
    idle_restart: bool,

    // Baseline level delivery rate for full pipe estimator.
    full_bw: u64,

    // The number of round for full pipe estimator without much growth.
    full_bw_count: usize,

    // Last time cycle_index is updated.
    cycle_stamp: Instant,

    // Current index of pacing_gain_cycle[].
    cycle_index: usize,

    // The upper bound on the volume of data BBR allows in flight.
    target_cwnd: usize,

    // Whether in the recovery episode.
    in_recovery: bool,

    // Start time of the connection.
    start_time: Instant,

    // Newly marked lost data size in bytes.
    newly_lost_bytes: usize,

    // Newly acked data size in bytes.
    newly_acked_bytes: usize,

    // bytes_in_flight before processing this ACK.
    prior_bytes_in_flight: usize,
}

impl State {
    pub fn new() -> Self {
        let now = Instant::now();

        State {
            state: BBRStateMachine::Startup,

            pacing_rate: 0,

            btlbw: 0,

            btlbwfilter: Minmax::new(0),

            rtprop: Duration::ZERO,

            rtprop_stamp: now,

            rtprop_expired: false,

            pacing_gain: 0.0,

            cwnd_gain: 0.0,

            filled_pipe: false,

            round_count: 0,

            round_start: false,

            next_round_delivered: 0,

            probe_rtt_done_stamp: None,

            probe_rtt_round_done: false,

            packet_conservation: false,

            prior_cwnd: 0,

            idle_restart: false,

            full_bw: 0,

            full_bw_count: 0,

            cycle_stamp: now,

            cycle_index: 0,

            target_cwnd: 0,

            in_recovery: false,

            start_time: now,

            newly_lost_bytes: 0,

            newly_acked_bytes: 0,

            prior_bytes_in_flight: 0,
        }
    }
}

// When entering the recovery episode.
fn bbr_enter_recovery(r: &mut Congestion, in_flight: usize, now: Instant) {
    r.bbr_state.prior_cwnd = per_ack::bbr_save_cwnd(r);

    r.congestion_window = in_flight.max(r.max_datagram_size);
    r.congestion_recovery_start_time = Some(now);

    r.bbr_state.packet_conservation = true;
    r.bbr_state.in_recovery = true;

    r.bbr_state.newly_lost_bytes = 0;

    // Start round now.
    r.bbr_state.next_round_delivered = r.delivery_rate.delivered();
}

// When exiting the recovery episode.
fn bbr_exit_recovery(r: &mut Congestion) {
    r.congestion_recovery_start_time = None;

    r.bbr_state.packet_conservation = false;
    r.bbr_state.in_recovery = false;

    per_ack::bbr_restore_cwnd(r);
}

// Congestion Control Hooks.
//
fn on_init(r: &mut Congestion) {
    init::bbr_init(r);
}

fn on_packet_sent(
    r: &mut Congestion, _sent_bytes: usize, bytes_in_flight: usize, _now: Instant,
) {
    per_transmit::bbr_on_transmit(r, bytes_in_flight);
}

fn on_packets_acked(
    r: &mut Congestion, bytes_in_flight: usize, packets: &mut Vec<Acked>,
    now: Instant, _rtt_stats: &RttStats,
) {
    r.bbr_state.prior_bytes_in_flight = bytes_in_flight;

    r.bbr_state.newly_acked_bytes =
        packets.drain(..).fold(0, |acked_bytes, p| {
            r.bbr_state.prior_bytes_in_flight -= p.size;

            per_ack::bbr_update_model_and_state(r, &p, bytes_in_flight, now);

            acked_bytes + p.size
        });

    if let Some(pkt) = packets.last() {
        if !r.in_congestion_recovery(pkt.time_sent) && r.bbr_state.in_recovery {
            // Upon exiting loss recovery.
            bbr_exit_recovery(r);
        }
    }

    per_ack::bbr_update_control_parameters(r, bytes_in_flight, now);

    r.bbr_state.newly_lost_bytes = 0;
}

fn congestion_event(
    r: &mut Congestion, bytes_in_flight: usize, lost_bytes: usize,
    largest_lost_pkt: &Sent, now: Instant,
) {
    r.bbr_state.newly_lost_bytes = lost_bytes;

    // Upon entering Fast Recovery.
    if !r.in_congestion_recovery(largest_lost_pkt.time_sent) {
        // Upon entering Fast Recovery.
        bbr_enter_recovery(r, bytes_in_flight - lost_bytes, now);
    }
}

fn checkpoint(_r: &mut Congestion) {}

fn rollback(_r: &mut Congestion) -> bool {
    false
}

fn has_custom_pacing() -> bool {
    true
}

#[cfg(feature = "qlog")]
fn state_str(r: &Congestion, _now: Instant) -> &'static str {
    r.bbr_state.state.into()
}

fn debug_fmt(r: &Congestion, f: &mut std::fmt::Formatter) -> std::fmt::Result {
    let bbr = &r.bbr_state;

    write!(
         f,
         "bbr={{ state={:?} btlbw={} rtprop={:?} pacing_rate={} pacing_gain={} cwnd_gain={} target_cwnd={} send_quantum={} filled_pipe={} round_count={} }}",
         bbr.state, bbr.btlbw, bbr.rtprop, bbr.pacing_rate, bbr.pacing_gain, bbr.cwnd_gain, bbr.target_cwnd, r.send_quantum(), bbr.filled_pipe, bbr.round_count
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::packet;
    use crate::ranges;
    use crate::recovery::congestion::recovery::LegacyRecovery;
    use crate::recovery::congestion::test_sender::TestSender;
    use crate::recovery::HandshakeStatus;
    use crate::recovery::RecoveryOps;
    use crate::OnAckReceivedOutcome;

    use smallvec::smallvec;

    fn test_sender() -> TestSender {
        TestSender::new(CongestionControlAlgorithm::BBR, false)
    }

    #[test]
    fn bbr_init() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(CongestionControlAlgorithm::BBR);

        let r = LegacyRecovery::new(&cfg);

        assert_eq!(
            r.cwnd(),
            r.max_datagram_size * cfg.initial_congestion_window_packets
        );
        assert_eq!(r.bytes_in_flight(), 0);

        assert_eq!(r.congestion.bbr_state.state, BBRStateMachine::Startup);
    }

    #[test]
    fn bbr_startup() {
        let mut sender = test_sender();
        let mss = sender.max_datagram_size;

        let rtt = Duration::from_millis(50);
        sender.update_rtt(rtt);
        sender.advance_time(rtt);

        // Send 5 packets.
        for _ in 0..5 {
            sender.send_packet(mss);
        }

        sender.advance_time(rtt);

        let cwnd_prev = sender.congestion_window;

        sender.ack_n_packets(5, mss);

        assert_eq!(sender.bbr_state.state, BBRStateMachine::Startup);
        assert_eq!(sender.congestion_window, cwnd_prev + mss * 5);
        assert_eq!(sender.bytes_in_flight, 0);
        assert_eq!(
            sender.delivery_rate().to_bytes_per_second(),
            ((mss * 5) as f64 / rtt.as_secs_f64()) as u64
        );
        assert_eq!(
            sender.bbr_state.btlbw,
            sender.delivery_rate().to_bytes_per_second()
        );
    }

    #[test]
    fn bbr_congestion_event() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(CongestionControlAlgorithm::BBR);

        let mut r = LegacyRecovery::new(&cfg);
        let now = Instant::now();
        let mss = r.max_datagram_size;

        // Send 5 packets.
        for pn in 0..5 {
            let pkt = Sent {
                pkt_num: pn,
                frames: smallvec![],
                time_sent: now,
                time_acked: None,
                time_lost: None,
                size: mss,
                ack_eliciting: true,
                in_flight: true,
                delivered: 0,
                delivered_time: now,
                first_sent_time: now,
                is_app_limited: false,
                tx_in_flight: 0,
                lost: 0,
                has_data: false,
                is_pmtud_probe: false,
            };

            r.on_packet_sent(
                pkt,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                "",
            );
        }

        let rtt = Duration::from_millis(50);
        let now = now + rtt;

        // Make a packet loss to trigger a congestion event.
        let mut acked = ranges::RangeSet::default();
        acked.insert(4..5);

        // 1 acked, 2 x MSS lost.
        assert_eq!(
            r.on_ack_received(
                &acked,
                25,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                None,
                "",
            )
            .unwrap(),
            OnAckReceivedOutcome {
                lost_packets: 2,
                lost_bytes: 2 * mss,
                acked_bytes: mss,
                spurious_losses: 0,
            },
        );

        // Sent: 0, 1, 2, 3, 4, Acked 4.
        assert_eq!(r.cwnd(), mss * 4);
        // Stil in flight: 2, 3.
        assert_eq!(r.bytes_in_flight(), mss * 2);
    }

    #[test]
    fn bbr_drain() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(CongestionControlAlgorithm::BBR);

        let mut r = LegacyRecovery::new(&cfg);
        let now = Instant::now();
        let mss = r.max_datagram_size;

        let mut pn = 0;

        // Stop right before filled_pipe=true.
        for _ in 0..3 {
            let pkt = Sent {
                pkt_num: pn,
                frames: smallvec![],
                time_sent: now,
                time_acked: None,
                time_lost: None,
                size: mss,
                ack_eliciting: true,
                in_flight: true,
                delivered: r.congestion.delivery_rate.delivered(),
                delivered_time: now,
                first_sent_time: now,
                is_app_limited: false,
                tx_in_flight: 0,
                lost: 0,
                has_data: false,
                is_pmtud_probe: false,
            };

            r.on_packet_sent(
                pkt,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                "",
            );

            pn += 1;

            let rtt = Duration::from_millis(50);

            let now = now + rtt;

            let mut acked = ranges::RangeSet::default();
            acked.insert(0..pn);

            assert_eq!(
                r.on_ack_received(
                    &acked,
                    25,
                    packet::Epoch::Application,
                    HandshakeStatus::default(),
                    now,
                    None,
                    "",
                )
                .unwrap(),
                OnAckReceivedOutcome {
                    lost_packets: 0,
                    lost_bytes: 0,
                    acked_bytes: mss,
                    spurious_losses: 0,
                },
            );
        }

        for _ in 0..7 {
            let pkt = Sent {
                pkt_num: pn,
                frames: smallvec![],
                time_sent: now,
                time_acked: None,
                time_lost: None,
                size: mss,
                ack_eliciting: true,
                in_flight: true,
                delivered: r.congestion.delivery_rate.delivered(),
                delivered_time: now,
                first_sent_time: now,
                is_app_limited: false,
                tx_in_flight: 0,
                lost: 0,
                has_data: false,
                is_pmtud_probe: false,
            };

            r.on_packet_sent(
                pkt,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                "",
            );

            pn += 1;
        }

        let rtt = Duration::from_millis(50);
        let now = now + rtt;

        let mut acked = ranges::RangeSet::default();

        // We sent 7 packets, but ack only one, to stay in Drain state.
        acked.insert(0..pn - 6);

        assert_eq!(
            r.on_ack_received(
                &acked,
                25,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                None,
                "",
            )
            .unwrap(),
            OnAckReceivedOutcome {
                lost_packets: 0,
                lost_bytes: 0,
                acked_bytes: mss,
                spurious_losses: 0,
            },
        );

        // Now we are in Drain state.
        assert!(r.congestion.bbr_state.filled_pipe);
        assert_eq!(r.congestion.bbr_state.state, BBRStateMachine::Drain);
        assert!(r.congestion.bbr_state.pacing_gain < 1.0);
    }

    #[test]
    fn bbr_probe_bw() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(CongestionControlAlgorithm::BBR);

        let mut r = LegacyRecovery::new(&cfg);
        let now = Instant::now();
        let mss = r.max_datagram_size;

        // At 4th roundtrip, filled_pipe=true and switch to Drain,
        // but move to ProbeBW immediately because bytes_in_flight is
        // smaller than BBRInFlight(1).
        for (pn, _) in (0..4).enumerate() {
            let pkt = Sent {
                pkt_num: pn as u64,
                frames: smallvec![],
                time_sent: now,
                time_acked: None,
                time_lost: None,
                size: mss,
                ack_eliciting: true,
                in_flight: true,
                delivered: r.congestion.delivery_rate.delivered(),
                delivered_time: now,
                first_sent_time: now,
                is_app_limited: false,
                tx_in_flight: 0,
                lost: 0,
                has_data: false,
                is_pmtud_probe: false,
            };

            r.on_packet_sent(
                pkt,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                "",
            );

            let rtt = Duration::from_millis(50);
            let now = now + rtt;

            let mut acked = ranges::RangeSet::default();
            acked.insert(0..pn as u64 + 1);

            assert_eq!(
                r.on_ack_received(
                    &acked,
                    25,
                    packet::Epoch::Application,
                    HandshakeStatus::default(),
                    now,
                    None,
                    "",
                )
                .unwrap(),
                OnAckReceivedOutcome {
                    lost_packets: 0,
                    lost_bytes: 0,
                    acked_bytes: mss,
                    spurious_losses: 0,
                },
            );
        }

        // Now we are in ProbeBW state.
        assert!(r.congestion.bbr_state.filled_pipe);
        assert_eq!(r.congestion.bbr_state.state, BBRStateMachine::ProbeBW);

        // In the first ProbeBW cycle, pacing_gain should be >= 1.0.
        assert!(r.congestion.bbr_state.pacing_gain >= 1.0);
    }

    #[test]
    fn bbr_probe_rtt() {
        let mut cfg = crate::Config::new(crate::PROTOCOL_VERSION).unwrap();
        cfg.set_cc_algorithm(CongestionControlAlgorithm::BBR);

        let mut r = LegacyRecovery::new(&cfg);
        let now = Instant::now();
        let mss = r.max_datagram_size;

        let mut pn = 0;

        // At 4th roundtrip, filled_pipe=true and switch to Drain,
        // but move to ProbeBW immediately because bytes_in_flight is
        // smaller than BBRInFlight(1).
        for _ in 0..4 {
            let pkt = Sent {
                pkt_num: pn,
                frames: smallvec![],
                time_sent: now,
                time_acked: None,
                time_lost: None,
                size: mss,
                ack_eliciting: true,
                in_flight: true,
                delivered: r.congestion.delivery_rate.delivered(),
                delivered_time: now,
                first_sent_time: now,
                is_app_limited: false,
                tx_in_flight: 0,
                lost: 0,
                has_data: false,
                is_pmtud_probe: false,
            };

            r.on_packet_sent(
                pkt,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                "",
            );

            pn += 1;

            let rtt = Duration::from_millis(50);
            let now = now + rtt;

            let mut acked = ranges::RangeSet::default();
            acked.insert(0..pn);

            assert_eq!(
                r.on_ack_received(
                    &acked,
                    25,
                    packet::Epoch::Application,
                    HandshakeStatus::default(),
                    now,
                    None,
                    "",
                )
                .unwrap(),
                OnAckReceivedOutcome {
                    lost_packets: 0,
                    lost_bytes: 0,
                    acked_bytes: mss,
                    spurious_losses: 0,
                },
            );
        }

        // Now we are in ProbeBW state.
        assert_eq!(r.congestion.bbr_state.state, BBRStateMachine::ProbeBW);

        // After RTPROP_FILTER_LEN (10s), switch to ProbeRTT.
        let now = now + RTPROP_FILTER_LEN;

        let pkt = Sent {
            pkt_num: pn,
            frames: smallvec![],
            time_sent: now,
            time_acked: None,
            time_lost: None,
            size: mss,
            ack_eliciting: true,
            in_flight: true,
            delivered: r.congestion.delivery_rate.delivered(),
            delivered_time: now,
            first_sent_time: now,
            is_app_limited: false,
            tx_in_flight: 0,
            lost: 0,
            has_data: false,
            is_pmtud_probe: false,
        };

        r.on_packet_sent(
            pkt,
            packet::Epoch::Application,
            HandshakeStatus::default(),
            now,
            "",
        );

        pn += 1;

        // Don't update rtprop by giving larger rtt than before.
        // If rtprop is updated, rtprop expiry check is reset.
        let rtt = Duration::from_millis(100);
        let now = now + rtt;

        let mut acked = ranges::RangeSet::default();
        acked.insert(0..pn);

        assert_eq!(
            r.on_ack_received(
                &acked,
                25,
                packet::Epoch::Application,
                HandshakeStatus::default(),
                now,
                None,
                "",
            )
            .unwrap(),
            OnAckReceivedOutcome {
                lost_packets: 0,
                lost_bytes: 0,
                acked_bytes: mss,
                spurious_losses: 0,
            },
        );

        assert_eq!(r.congestion.bbr_state.state, BBRStateMachine::ProbeRTT);
        assert_eq!(r.congestion.bbr_state.pacing_gain, 1.0);
    }
}

mod init;
mod pacing;
mod per_ack;
mod per_transmit;
