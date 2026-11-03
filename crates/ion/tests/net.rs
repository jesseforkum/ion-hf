//! Net mode on the simulated air: check-ins, polled rounds, relays and
//! bulletins.

use std::sync::Arc;

use ion::link::Air;
use ion::link::net::{MAX_LINE, NetConfig, NetEvent, NetStation, ObjectId, Precedence, Traffic};
use ion::link::net_sim::NetSim;
use ion::link::sim::ModelAir;

const NCS: &str = "N0NCS";
const NET: &str = "EMCOMM";

/// NCS is station 0; the members follow, all at `snr_db` to each other.
fn net(members: &[&str], snr_db: f32, seed: u64, tweak: impl Fn(&mut NetConfig)) -> NetSim {
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let mut cfg = NetConfig::new(NCS, NET, true);
    tweak(&mut cfg);
    let mut stations = vec![NetStation::new(cfg, air.clone())];
    for (i, call) in members.iter().enumerate() {
        let mut cfg = NetConfig::new(call, NET, false);
        cfg.seed = seed.wrapping_mul(7919).wrapping_add(i as u64 + 1);
        tweak(&mut cfg);
        stations.push(NetStation::new(cfg, air.clone()));
    }
    NetSim::new(stations, snr_db, air, seed)
}

/// Runs, collecting every station's events.
fn run(
    sim: &mut NetSim,
    events: &mut [Vec<NetEvent>],
    t_max: f64,
    mut done: impl FnMut(&[Vec<NetEvent>]) -> bool,
) -> bool {
    while sim.t < t_max {
        for (i, s) in sim.stations.iter_mut().enumerate() {
            events[i].extend(s.take_events());
        }
        if done(events) {
            return true;
        }
        for _ in 0..100 {
            sim.step();
        }
    }
    false
}

fn messages_from(ev: &[NetEvent], from: &str) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            NetEvent::Message {
                from: f,
                data,
                overheard: false,
                ..
            } if f == from => Some(String::from_utf8_lossy(data).to_string()),
            _ => None,
        })
        .collect()
}

fn bulletins(ev: &[NetEvent]) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            NetEvent::Message { to: None, data, .. } => Some(String::from_utf8_lossy(data).to_string()),
            _ => None,
        })
        .collect()
}

fn report(call: &str) -> String {
    format!("{call} checking in from the county EOC. No traffic. Power and phones up. 73")
}

const BULLETIN: &str = "QST all stations: red flag warning for Wilson and Lincoln counties until 8 PM. \
Shelters open at the Central civic center. Next net 2100 local on 3.576. de N0NCS";

#[test]
fn a_net_checks_in_reports_and_gets_the_bulletin() {
    let calls = ["K1ABC", "W1AW", "N0CALL", "W5XYZ", "N0ABC"];
    let mut sim = net(&calls, 8.0, 1, |_| {});
    for (i, c) in calls.iter().enumerate() {
        sim.stations[i + 1].send(report(c).as_bytes());
    }
    sim.stations[0].send(BULLETIN.as_bytes());
    let mut ev = vec![Vec::new(); calls.len() + 1];
    let ok = run(&mut sim, &mut ev, 1800.0, |ev| {
        calls.iter().all(|c| messages_from(&ev[0], c).len() == 1) && ev[1..].iter().all(|e| bulletins(e).len() == 1)
    });
    let roster = sim.stations[0].roster(sim.t);
    assert!(ok, "after {:.0} s: roster {roster:#?}\nNCS events {:#?}", sim.t, ev[0]);
    for (i, c) in calls.iter().enumerate() {
        assert_eq!(messages_from(&ev[0], c), vec![report(c)]);
        assert_eq!(bulletins(&ev[i + 1]), vec![BULLETIN.to_string()]);
        assert!(
            ev[i + 1].iter().any(|e| matches!(e, NetEvent::Joined { .. })),
            "{c} never joined"
        );
    }
    // Members also copy each other's reports, as on a voice net.
    let overheard = ev[1]
        .iter()
        .filter(|e| matches!(e, NetEvent::Message { overheard: true, .. }))
        .count();
    assert!(overheard >= 3, "K1ABC overheard only {overheard} reports");
}

#[test]
fn a_weak_station_gets_through_at_the_floor() {
    let calls = ["K1ABC", "W1AW", "W5WEAK"];
    let mut sim = net(&calls, 12.0, 2, |_| {});
    sim.set_path(0, 3, -9.0);
    sim.stations[3].send(b"W5WEAK on battery power, all well here");
    sim.stations[1].send(&vec![b'x'; 1500]);
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 2400.0, |ev| {
        !messages_from(&ev[0], "W5WEAK").is_empty() && !messages_from(&ev[0], "K1ABC").is_empty()
    });
    assert!(ok, "roster {:#?}", sim.stations[0].roster(sim.t));
    let roster = sim.stations[0].roster(sim.t);
    let weak = roster.iter().find(|r| r.call == "W5WEAK").unwrap();
    let strong = roster.iter().find(|r| r.call == "K1ABC").unwrap();
    assert!(weak.cap < strong.cap, "weak {weak:?} strong {strong:?}");
}

#[test]
fn a_station_only_another_member_hears_is_relayed() {
    // N5HID hears NCS fine but NCS cannot hear it; W1AW hears both.
    let calls = ["W1AW", "K1ABC", "N5HID"];
    let mut sim = net(&calls, 10.0, 3, |_| {});
    sim.snr[3][0] = -40.0; // N5HID → NCS: nothing
    sim.snr[0][3] = 6.0; // NCS → N5HID: fine
    sim.set_path(2, 3, -40.0); // K1ABC cannot hear it either
    sim.stations[3].send(b"N5HID: tree down on highway 12, road blocked");
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 3000.0, |ev| {
        !messages_from(&ev[0], "N5HID").is_empty()
    });
    assert!(
        ok,
        "NCS events {:#?}\nroster {:#?}",
        ev[0],
        sim.stations[0].roster(sim.t)
    );
    assert!(
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::CheckedIn { call, via: Some(v), .. } if call == "N5HID" && v == "W1AW")),
        "check-in not relayed: {:#?}",
        ev[0]
    );
    assert!(
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::Relayed { source, via } if source == "N5HID" && via == "W1AW")),
        "{:#?}",
        ev[0]
    );
    assert_eq!(
        messages_from(&ev[0], "N5HID"),
        vec!["N5HID: tree down on highway 12, road blocked".to_string()]
    );
}

#[test]
fn a_pile_up_of_check_ins_sorts_itself_out() {
    let calls: Vec<String> = (0..12)
        .map(|i| format!("W{}ABC", i % 10) + &"XYZ"[..(i / 10) + 1])
        .collect();
    let refs: Vec<&str> = calls.iter().map(String::as_str).collect();
    let mut sim = net(&refs, 10.0, 4, |c| c.beacon_every_s = 90.0);
    let mut ev = vec![Vec::new(); calls.len() + 1];
    let ok = run(&mut sim, &mut ev, 3600.0, |ev| {
        ev[1..]
            .iter()
            .all(|e| e.iter().any(|x| matches!(x, NetEvent::Joined { .. })))
    });
    let joined: Vec<&str> = refs
        .iter()
        .enumerate()
        .filter(|(i, _)| ev[i + 1].iter().any(|x| matches!(x, NetEvent::Joined { .. })))
        .map(|(_, c)| *c)
        .collect();
    assert!(
        ok,
        "only {} of 12 joined after {:.0} s: {joined:?}",
        joined.len(),
        sim.t
    );
    let slots: std::collections::BTreeSet<u8> = sim.stations[1..].iter().filter_map(|s| s.slot()).collect();
    assert_eq!(slots.len(), 12, "slots not unique");
}

#[test]
fn a_bulletin_reaches_everyone_through_fading() {
    let calls = ["K1ABC", "W1AW", "N0CALL", "W5XYZ"];
    let mut sim = net(&calls, 6.0, 5, |_| {});
    sim.fade_db = 6.0;
    sim.fade_tau_s = 15.0;
    let long: String = (0..12).map(|i| format!("{i:02} {BULLETIN} ")).collect();
    sim.stations[0].send(long.as_bytes());
    let mut ev = vec![Vec::new(); 5];
    let ok = run(&mut sim, &mut ev, 3600.0, |ev| {
        ev[1..].iter().all(|e| bulletins(e).len() == 1)
    });
    assert!(
        ok,
        "after {:.0} s: {:?}",
        sim.t,
        ev[1..].iter().map(|e| bulletins(e).len()).collect::<Vec<_>>()
    );
    for e in &ev[1..] {
        assert_eq!(bulletins(e), vec![long.clone()]);
    }
}

#[test]
fn direct_messages_are_compressed_for_members_that_announce_it() {
    // NCS and K1ABC have compression on, W1AW not: NCS packs only K1ABC's
    // messages, and both get exactly what was sent.
    let calls = ["K1ABC", "W1AW"];
    let mut sim = net(&calls, 10.0, 6, |c| c.compress = c.call != "W1AW");
    let mut ev = vec![Vec::new(); 3];
    assert!(run(&mut sim, &mut ev, 900.0, |ev| ev[1..]
        .iter()
        .all(|e| e.iter().any(|e| matches!(e, NetEvent::Joined { .. })))));
    let texts: [&[u8]; 3] = [
        b"K1ABC, please proceed to the Eastside shelter and relieve N1CALL at 2200.",
        &[0x00, 0xFF, 0x80, 0x7F, 0x01],
        b"",
    ];
    for t in texts {
        assert!(sim.stations[0].send_to("K1ABC", t));
        assert!(sim.stations[0].send_to("W1AW", t));
    }
    let got = |ev: &[NetEvent], call: &str| -> Vec<Vec<u8>> {
        ev.iter()
            .filter_map(|e| match e {
                NetEvent::Message {
                    to: Some(t),
                    data,
                    overheard: false,
                    ..
                } if t == call => Some(data.clone()),
                _ => None,
            })
            .collect()
    };
    let ok = run(&mut sim, &mut ev, 2400.0, |ev| {
        got(&ev[1], "K1ABC").len() == 3 && got(&ev[2], "W1AW").len() == 3
    });
    assert!(ok, "K1ABC {:?}\nW1AW {:?}", got(&ev[1], "K1ABC"), got(&ev[2], "W1AW"));
    let want: Vec<Vec<u8>> = texts.iter().map(|t| t.to_vec()).collect();
    assert_eq!(got(&ev[1], "K1ABC"), want);
    assert_eq!(got(&ev[2], "W1AW"), want);
}

#[test]
fn ncs_can_message_one_member_and_close_the_net() {
    let calls = ["K1ABC", "W1AW"];
    let mut sim = net(&calls, 10.0, 6, |_| {});
    let mut ev = vec![Vec::new(); 3];
    assert!(run(&mut sim, &mut ev, 900.0, |ev| ev[1]
        .iter()
        .any(|e| matches!(e, NetEvent::Joined { .. }))));
    assert!(sim.stations[0].send_to("K1ABC", b"K1ABC please QSY to 3.580 for traffic"));
    assert!(!sim.stations[0].send_to("NOBODY", b"x"));
    let ok = run(&mut sim, &mut ev, 1800.0, |ev| {
        ev[1]
            .iter()
            .any(|e| matches!(e, NetEvent::Message { to: Some(t), .. } if t == "K1ABC"))
    });
    assert!(ok);
    assert!(
        !ev[2]
            .iter()
            .any(|e| matches!(e, NetEvent::Message { to: Some(t), overheard: false, .. } if t == "W1AW"))
    );
    sim.stations[0].close();
    let until = sim.t + 120.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        ev[1..].iter().all(|e| e.contains(&NetEvent::Closed))
    });
    assert!(ok, "members never heard the close");
}

/// Runs until every member other than `call` reports hearing it.
fn until_all_hear(sim: &mut NetSim, call: &str, t_max: f64) -> bool {
    let n = sim.stations.len() - 1;
    sim.run_until(t_max, |s| {
        let r = s.stations[0].roster(s.t);
        r.len() == n
            && r.iter()
                .filter(|e| e.call != call)
                .all(|e| e.hears.iter().any(|(c, _)| c == call))
    })
}

#[test]
fn a_message_ncs_missed_is_recovered_through_the_received_list() {
    // N5LOST checks in normally; then NCS stops hearing it (it still hears
    // NCS). W1AW and K1ABC both copy what it sends.
    let calls = ["N5LOST", "W1AW", "K1ABC"];
    let mut sim = net(&calls, 10.0, 7, |_| {});
    assert!(
        until_all_hear(&mut sim, "N5LOST", 1500.0),
        "roster {:#?}",
        sim.stations[0].roster(sim.t)
    );
    sim.snr[1][0] = -40.0;
    let msg = "N5LOST: water over the road at the Stone River bridge, closed both ways";
    sim.stations[1].send(msg.as_bytes());
    let mut ev = vec![Vec::new(); 4];
    let t = sim.t;
    let ok = run(&mut sim, &mut ev, t + 1800.0, |ev| {
        !messages_from(&ev[0], "N5LOST").is_empty()
    });
    assert!(
        ok,
        "NCS events {:#?}\nroster {:#?}",
        ev[0],
        sim.stations[0].roster(sim.t)
    );
    // N5LOST learns from the received list that NCS has it, though NCS
    // never acknowledged a burst of its.
    let until = sim.t + 900.0;
    assert!(
        sim.run_until(until, |s| s.stations[1].pending() == 0),
        "N5LOST still holds it"
    );
    assert!(sim.stations[0].stats.rx_lists >= 1);
    for e in &mut ev {
        e.clear();
    }
    let until = sim.t + 300.0;
    run(&mut sim, &mut ev, until, |_| false);
    let relayers: Vec<&str> = (2..4)
        .filter(|&i| sim.stations[i].stats.relayed_frames > 0)
        .map(|i| calls[i - 1])
        .collect();
    assert_eq!(relayers.len(), 1, "relayed by {relayers:?}");
}

#[test]
fn capability_flags_reach_the_roster_and_steer_relays() {
    use ion::link::net::Caps;
    // W5NO hears N5LOST best but will not relay; W5YES will.
    let calls = ["N5LOST", "W5NO", "W5YES"];
    let no = Caps {
        qsy: true,
        wide: true,
        relay: false,
        attended: false,
        chorus: false,
    };
    let air: Arc<dyn Air> = Arc::new(ModelAir::default());
    let mut stations = vec![NetStation::new(NetConfig::new(NCS, NET, true), air.clone())];
    for (i, call) in calls.iter().enumerate() {
        let mut cfg = NetConfig::new(call, NET, false);
        cfg.seed = 900 + i as u64;
        if *call == "W5NO" {
            cfg.caps = no;
            cfg.version = 0x23;
        }
        stations.push(NetStation::new(cfg, air.clone()));
    }
    let mut sim = NetSim::new(stations, 10.0, air, 8);
    sim.set_path(1, 2, 16.0);
    assert!(until_all_hear(&mut sim, "N5LOST", 1500.0));
    let roster = sim.stations[0].roster(sim.t);
    let w5no = roster.iter().find(|r| r.call == "W5NO").unwrap();
    assert_eq!((w5no.caps, w5no.version), (Some(no), Some(0x23)));
    let w5yes = roster.iter().find(|r| r.call == "W5YES").unwrap();
    assert_eq!(w5yes.caps, Some(Caps::default()));
    sim.snr[1][0] = -40.0;
    sim.stations[1].send(b"N5LOST: generator down, on battery");
    let mut ev = vec![Vec::new(); 4];
    let until = sim.t + 1800.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        !messages_from(&ev[0], "N5LOST").is_empty()
    });
    assert!(ok, "{:#?}", ev[0]);
    assert!(
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::Relayed { via, .. } if via == "W5YES")),
        "{:#?}",
        ev[0]
    );
    assert_eq!(sim.stations[2].stats.relayed_frames, 0);
}

/// A net where `leaf` (the last station) hears only the stations in
/// `hears` (index, SNR); nobody else hears it, and it hears nobody else.
fn tree_net(calls: &[&str], hears: &[(usize, f32)], seed: u64) -> NetSim {
    let mut sim = net(calls, 10.0, seed, |c| {
        c.seek_after_s = 60.0;
        c.seek_every_s = 40.0;
    });
    let leaf = calls.len();
    for i in 0..leaf {
        sim.set_path(i, leaf, -40.0);
    }
    for &(i, snr) in hears {
        sim.set_path(i, leaf, snr);
    }
    sim
}

#[test]
fn a_station_that_cannot_hear_ncs_joins_through_a_relay() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 8.0)], 9);
    sim.stations[3].send(b"W5FAR: county road 12 washed out, sending photos by courier");
    sim.stations[0].send(BULLETIN.as_bytes());
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 3000.0, |ev| {
        ev[3].iter().any(|e| matches!(e, NetEvent::Joined { .. })) && !messages_from(&ev[0], "W5FAR").is_empty()
    });
    assert!(
        ok,
        "after {:.0} s\nNCS {:#?}\nW5FAR {:#?}\nroster {:#?}",
        sim.t,
        ev[0],
        ev[3],
        sim.stations[0].roster(sim.t)
    );
    assert!(
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::Relayed { source, via } if source == "W5FAR" && via == "W1AW")),
        "{:#?}",
        ev[0]
    );
    let roster = sim.stations[0].roster(sim.t);
    let far = roster.iter().find(|r| r.call == "W5FAR").unwrap();
    assert_eq!(far.via.as_deref(), Some("W1AW"));
    assert!(
        roster
            .iter()
            .any(|r| r.call == "W1AW" && r.children == vec!["W5FAR".to_string()])
    );
    assert_eq!(sim.stations[3].ncs_call(), Some(NCS));
    // A bulletin and a message for it, through its relay.
    assert!(sim.stations[0].send_to("W5FAR", b"W5FAR copy, courier on the way"));
    let until = sim.t + 2400.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        bulletins(&ev[3]).len() == 1
            && ev[3]
                .iter()
                .any(|e| matches!(e, NetEvent::Message { to: Some(t), .. } if t == "W5FAR"))
    });
    assert!(ok, "after {:.0} s\nW5FAR {:#?}", sim.t, ev[3]);
    assert_eq!(bulletins(&ev[3]), vec![BULLETIN.to_string()]);
}

#[test]
fn a_leaf_is_moved_when_its_relay_goes_silent() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 10.0), (2, 6.0)], 10);
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 3000.0, |ev| {
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::CheckedIn { call, via: Some(_), .. } if call == "W5FAR"))
    });
    assert!(ok, "{:#?}", ev[0]);
    let first = sim.stations[0]
        .roster(sim.t)
        .into_iter()
        .find(|r| r.call == "W5FAR")
        .unwrap()
        .via
        .unwrap();
    let (dead, other) = if first == "W1AW" { (1, "K1ABC") } else { (2, "W1AW") };
    // Its relay must hear it answering first, and the other member too.
    let until = sim.t + 600.0;
    sim.run_until(until, |s| {
        s.stations[0]
            .roster(s.t)
            .iter()
            .any(|r| r.call == other && r.hears.iter().any(|(c, _)| c == "W5FAR"))
    });
    for i in 0..4 {
        if i != dead {
            sim.set_path(dead, i, -40.0);
        }
    }
    sim.stations[3].send(b"W5FAR: still here, relay lost");
    let until = sim.t + 3000.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        !messages_from(&ev[0], "W5FAR").is_empty()
    });
    assert!(
        ok,
        "after {:.0} s\nNCS {:#?}\nroster {:#?}",
        sim.t,
        ev[0],
        sim.stations[0].roster(sim.t)
    );
    assert!(
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::Routed { call, via: Some(v) } if call == "W5FAR" && v == other)),
        "{:#?}",
        ev[0]
    );
    assert!(
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::Relayed { source, via } if source == "W5FAR" && via == other))
    );
}

/// A leaf NCS turns out to hear directly (say the path opened, or the leaf
/// only joined through a relay because its check-in collided) leaves its
/// relay and becomes an ordinary member, so it can relay for others in turn.
#[test]
fn a_leaf_ncs_can_hear_becomes_direct() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 8.0)], 11);
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 3000.0, |ev| {
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::CheckedIn { call, via: Some(_), .. } if call == "W5FAR"))
    });
    assert!(ok, "{:#?}", ev[0]);
    // The path to NCS opens both ways.
    sim.set_path(0, 3, 10.0);
    sim.stations[3].send(b"W5FAR: band is up, hearing you direct now");
    let until = sim.t + 2400.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        ev[0]
            .iter()
            .any(|e| matches!(e, NetEvent::Routed { call, via: None } if call == "W5FAR"))
            && !messages_from(&ev[0], "W5FAR").is_empty()
    });
    assert!(
        ok,
        "after {:.0} s\nNCS {:#?}\nroster {:#?}",
        sim.t,
        ev[0],
        sim.stations[0].roster(sim.t)
    );
    let roster = sim.stations[0].roster(sim.t);
    assert_eq!(roster.iter().find(|r| r.call == "W5FAR").unwrap().via, None);
    // W5FAR hears that it has no relay any more (the next acks burst).
    let until = sim.t + 600.0;
    let ok = sim.run_until(until, |s| s.stations[3].relay_call().is_none());
    assert!(ok, "W5FAR still goes through {:?}", sim.stations[3].relay_call());
}

/// Bulletin repair of 50 pieces to 10 stations that each lose 10% of what
/// they hear: coded repair (as many fresh pieces as the station furthest
/// short needs) against resending from the lowest piece anyone lacks, and
/// against resending every piece someone lacks.
#[test]
fn coded_repair_needs_fewer_sends_than_resending() {
    use ion::link::net_code::{Decoder, PIECE, encode, sources};
    use ion_dsp::dsp::NoiseGen;
    let (k, stations, loss, trials) = (50usize, 10usize, 0.1f32, 400usize);
    let mut g = NoiseGen::new(11);
    let data: Vec<u8> = (0..k * PIECE).map(|i| (i * 7 + 3) as u8).collect();
    let src = sources(&data);
    let (mut rewind_total, mut sel_total, mut coded_total) = (0usize, 0usize, 0usize);
    for _ in 0..trials {
        // Go back to the lowest piece anyone lacks.
        let mut have = vec![vec![false; k]; stations];
        let mut from = 0;
        let mut sent = 0;
        while from < k {
            for i in from..k {
                sent += 1;
                for h in have.iter_mut() {
                    if g.uniform() >= loss {
                        h[i] = true;
                    }
                }
            }
            from = have
                .iter()
                .map(|h| h.iter().position(|x| !x).unwrap_or(k))
                .min()
                .unwrap();
        }
        rewind_total += sent - k;
        // Selective: every piece anyone lacks.
        let mut have = vec![vec![false; k]; stations];
        let mut want: Vec<usize> = (0..k).collect();
        let mut sent = 0;
        while !want.is_empty() {
            for &i in &want {
                sent += 1;
                for h in have.iter_mut() {
                    if g.uniform() >= loss {
                        h[i] = true;
                    }
                }
            }
            want = (0..k).filter(|&i| have.iter().any(|h| !h[i])).collect();
        }
        sel_total += sent - k;
        // Coded: the sources, then the largest shortfall in new pieces.
        let mut dec: Vec<Decoder> = (0..stations).map(|_| Decoder::new(k)).collect();
        let mut next = 0u32;
        let mut need = k;
        while need > 0 {
            for _ in 0..need {
                let piece = encode(&src, (next % 256) as u8);
                for d in dec.iter_mut() {
                    if g.uniform() >= loss {
                        d.add((next % 256) as u8, &piece);
                    }
                }
                next += 1;
            }
            need = dec.iter().map(Decoder::short).max().unwrap();
        }
        coded_total += next as usize - k;
        for d in &dec {
            assert_eq!(d.data(data.len()).unwrap(), data);
        }
    }
    let per = |x: usize| x as f64 / trials as f64;
    let (rewind, sel, coded) = (per(rewind_total), per(sel_total), per(coded_total));
    assert!(coded < sel / 2.5, "selective {sel:.1} coded {coded:.1}");
    assert!(coded < rewind / 2.5, "rewind {rewind:.1} coded {coded:.1}");
    assert!(coded < 15.0, "coded {coded:.1}");
}

fn joined(ev: &[NetEvent]) -> bool {
    ev.iter().any(|e| matches!(e, NetEvent::Joined { .. }))
}

fn lines(ev: &[NetEvent], call: &str) -> Vec<String> {
    ev.iter()
        .filter_map(|e| match e {
            NetEvent::Line { call: c, line } if c == call => Some(line.clone()),
            _ => None,
        })
        .collect()
}

fn traffic_of(ev: &[NetEvent], call: &str) -> Vec<Traffic> {
    ev.iter()
        .filter_map(|e| match e {
            NetEvent::Traffic { call: c, traffic } if c == call => Some(*traffic),
            _ => None,
        })
        .collect()
}

fn objects(ev: &[NetEvent]) -> Vec<(ObjectId, Vec<u8>, bool)> {
    ev.iter()
        .filter_map(|e| match e {
            NetEvent::Object { id, data, cached } => Some((*id, data.clone(), *cached)),
            _ => None,
        })
        .collect()
}

const LINE: &str = "Trousdale County ARES, EOC net control";

#[test]
fn traffic_and_lines_reach_the_roster() {
    let calls = ["K1ABC", "W1AW"];
    let listed = Traffic {
        items: 2,
        precedence: Precedence::Priority,
        short: true,
    };
    let mut sim = net(&calls, 10.0, 12, |c| {
        if c.call == "K1ABC" {
            c.line = LINE.into();
            c.traffic = listed;
        } else if c.call == "W1AW" {
            c.line = "Wilson Co.".into();
        }
    });
    assert_eq!(LINE.len(), MAX_LINE);
    let mut ev = vec![Vec::new(); 3];
    let ok = run(&mut sim, &mut ev, 1200.0, |ev| {
        lines(&ev[0], "K1ABC") == [LINE] && lines(&ev[0], "W1AW") == ["Wilson Co."]
    });
    assert!(ok, "after {:.0} s: NCS {:#?}", sim.t, ev[0]);
    // The traffic came with the check-in.
    assert_eq!(traffic_of(&ev[0], "K1ABC"), vec![listed]);
    assert!(traffic_of(&ev[0], "W1AW").is_empty());
    let roster = sim.stations[0].roster(sim.t);
    let k = roster.iter().find(|r| r.call == "K1ABC").unwrap();
    assert_eq!((k.line.as_str(), k.traffic), (LINE, listed));
    // Changed later: they come with the next report.
    let welfare = Traffic {
        items: 1,
        precedence: Precedence::Welfare,
        short: false,
    };
    sim.stations[2].set_traffic(welfare);
    sim.stations[2].set_line("Wilson County EMA");
    sim.stations[1].set_traffic(Traffic::default());
    let until = sim.t + 900.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        traffic_of(&ev[0], "W1AW") == [welfare]
            && lines(&ev[0], "W1AW").len() == 2
            && traffic_of(&ev[0], "K1ABC").len() == 2
    });
    assert!(ok, "after {:.0} s: NCS {:#?}", sim.t, ev[0]);
    assert_eq!(lines(&ev[0], "W1AW")[1], "Wilson County EMA");
    let roster = sim.stations[0].roster(sim.t);
    let w = roster.iter().find(|r| r.call == "W1AW").unwrap();
    assert_eq!((w.line.as_str(), w.traffic), ("Wilson County EMA", welfare));
    let k = roster.iter().find(|r| r.call == "K1ABC").unwrap();
    assert_eq!((k.line.as_str(), k.traffic), (LINE, Traffic::default()));
    // A line goes once.
    assert_eq!(lines(&ev[0], "K1ABC").len(), 1);
}

#[test]
fn a_leafs_line_and_traffic_come_through_its_relay() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 8.0)], 13);
    let urgent = Traffic {
        items: 1,
        precedence: Precedence::Emergency,
        short: false,
    };
    sim.stations[3].set_line(LINE);
    sim.stations[3].set_traffic(urgent);
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 3000.0, |ev| {
        joined(&ev[3]) && lines(&ev[0], "W5FAR") == [LINE] && traffic_of(&ev[0], "W5FAR") == [urgent]
    });
    assert!(ok, "after {:.0} s\nNCS {:#?}\nW5FAR {:#?}", sim.t, ev[0], ev[3]);
    let roster = sim.stations[0].roster(sim.t);
    let far = roster.iter().find(|r| r.call == "W5FAR").unwrap();
    assert_eq!(far.via.as_deref(), Some("W1AW"));
    assert_eq!((far.line.as_str(), far.traffic), (LINE, urgent));
}

#[test]
fn priority_and_emergency_members_are_polled_first() {
    let calls = ["K1ABC", "W1AW", "N0CALL", "W5XYZ", "N0ABC"];
    let tr = |p: Precedence, short: bool| Traffic {
        items: 1,
        precedence: p,
        short,
    };
    let mut sim = net(&calls, 10.0, 14, |c| {
        c.traffic = match c.call.as_str() {
            "N0ABC" => tr(Precedence::Emergency, false),
            "W5XYZ" => tr(Precedence::Priority, false),
            "N0CALL" => tr(Precedence::Routine, true),
            "W1AW" => tr(Precedence::Welfare, false),
            _ => Traffic::default(),
        }
    });
    let mut ev = vec![Vec::new(); calls.len() + 1];
    let ok = run(&mut sim, &mut ev, 1800.0, |ev| {
        ev[1..].iter().all(|e| joined(e)) && traffic_of(&ev[0], "N0ABC").len() == 1
    });
    assert!(ok, "after {:.0} s: NCS {:#?}", sim.t, ev[0]);
    // At the start of a round everyone queues one short message: they
    // arrive in the order NCS polls.
    let r = sim.stations[0].stats.rounds;
    let until = sim.t + 600.0;
    assert!(sim.run_until(until, |s| s.stations[0].stats.rounds > r));
    for (i, c) in calls.iter().enumerate() {
        sim.stations[i + 1].send(c.as_bytes());
    }
    ev[0].clear();
    let until = sim.t + 900.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        calls.iter().all(|c| messages_from(&ev[0], c).len() == 1)
    });
    assert!(ok, "after {:.0} s: NCS {:#?}", sim.t, ev[0]);
    let order: Vec<String> = ev[0]
        .iter()
        .filter_map(|e| match e {
            NetEvent::Message { from, .. } => Some(from.clone()),
            _ => None,
        })
        .collect();
    let mut rest: Vec<(u8, String)> = sim.stations[0]
        .roster(sim.t)
        .into_iter()
        .filter(|r| ["K1ABC", "W1AW"].contains(&r.call.as_str()))
        .map(|r| (r.slot, r.call))
        .collect();
    rest.sort();
    let mut want: Vec<String> = ["N0ABC", "W5XYZ", "N0CALL"].iter().map(|s| s.to_string()).collect();
    want.extend(rest.into_iter().map(|r| r.1));
    assert_eq!(order, want);
}

/// The store directory of one member in one test.
fn store(test: &str, call: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ion-net-{}-{test}-{call}", std::process::id()))
}

/// An announcement-sized object: about 6 kB, three blocks of the code.
fn announcement() -> Vec<u8> {
    let mut a = b"# Trousdale County ARES net\n\nNet control N0NCS, alternate W1AW.\n\n".to_vec();
    while a.len() < 6000 {
        a.extend(format!("| {} | shelter {} | open |\n", a.len(), a.len() % 97).as_bytes());
    }
    a
}

/// Pieces of the code an object of `len` bytes takes.
fn sources(len: usize) -> u32 {
    (0..len.div_ceil(2560))
        .map(|j| (len - j * 2560).min(2560).div_ceil(20) as u32)
        .sum()
}

/// A net with a store for each member, `held` holding `data` already,
/// run until everyone is in; then NCS offers `data`.
fn object_net(test: &str, calls: &[&str], held: &[&str], data: &[u8], seed: u64) -> (NetSim, Vec<Vec<NetEvent>>) {
    let id = ObjectId::of(data);
    let mut sim = net(calls, 10.0, seed, |c| {
        if !c.ncs {
            let dir = store(test, &c.call);
            let _ = std::fs::remove_dir_all(&dir);
            if held.contains(&c.call.as_str()) {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join(id.to_string()), data).unwrap();
            }
            c.store = Some(dir);
        }
    });
    let mut ev = vec![Vec::new(); calls.len() + 1];
    assert!(run(&mut sim, &mut ev, 1200.0, |ev| ev[1..].iter().all(|e| joined(e))));
    assert_eq!(sim.stations[0].offer(data), id);
    (sim, ev)
}

fn all_have_it(ev: &[Vec<NetEvent>]) -> bool {
    ev[1..].iter().all(|e| !objects(e).is_empty())
}

#[test]
fn an_offered_object_reaches_every_member_once() {
    let calls = ["K1ABC", "W1AW", "N0CALL"];
    let data = announcement();
    let id = ObjectId::of(&data);
    let (mut sim, mut ev) = object_net("once", &calls, &[], &data, 15);
    let t0 = sim.t;
    let ok = run(&mut sim, &mut ev, 2400.0, all_have_it);
    assert!(ok, "after {:.0} s: NCS {:#?}", sim.t, ev[0]);
    println!("object of {} bytes to 3 members: {:.0} s", data.len(), sim.t - t0);
    let until = sim.t + 600.0;
    run(&mut sim, &mut ev, until, |_| false);
    for (i, c) in calls.iter().enumerate() {
        assert_eq!(objects(&ev[i + 1]), vec![(id, data.clone(), false)], "{c}");
        // Kept in its store, named by the id.
        let file = store("once", c).join(id.to_string());
        assert_eq!(std::fs::read(&file).unwrap(), data);
        let _ = std::fs::remove_dir_all(store("once", c));
    }
    let roster = sim.stations[0].roster(sim.t);
    assert!(roster.iter().all(|r| r.holds == 1), "{roster:#?}");
    // Sent once for all of them: the sources, and few repairs.
    let st = &sim.stations[0].stats;
    let k = sources(data.len());
    println!("{} pieces ({} repairs) for {k} sources", st.obj_pieces, st.obj_repairs);
    assert!(
        st.obj_pieces >= k && st.obj_pieces < k + k / 4,
        "{} pieces for {k}",
        st.obj_pieces
    );
    assert_eq!(st.bul_pieces, 0);
}

#[test]
fn a_member_that_holds_the_object_costs_no_pieces() {
    let calls = ["K1ABC", "W1AW", "N0CALL"];
    let data = announcement();
    let id = ObjectId::of(&data);
    // Everyone holds it: nothing goes on the air but the offer.
    let (mut sim, mut ev) = object_net("held", &calls, &calls, &data, 16);
    assert!(run(&mut sim, &mut ev, 1800.0, all_have_it));
    let until = sim.t + 600.0;
    run(&mut sim, &mut ev, until, |_| false);
    for e in &ev[1..] {
        assert_eq!(objects(e), vec![(id, data.clone(), true)]);
    }
    assert_eq!(sim.stations[0].stats.obj_pieces, 0);
    assert!(sim.stations[0].roster(sim.t).iter().all(|r| r.holds == 1));
    // Only N0CALL lacks it: what it needs goes, once.
    let (mut sim, mut ev) = object_net("held1", &calls, &["K1ABC", "W1AW"], &data, 16);
    assert!(run(&mut sim, &mut ev, 2400.0, all_have_it));
    assert_eq!(objects(&ev[3]), vec![(id, data.clone(), false)]);
    let one = sim.stations[0].stats.obj_pieces;
    let k = sources(data.len());
    println!("one member lacking: {one} pieces for {k} sources");
    assert!(one >= k && one < k + k / 4, "{one} pieces for {k}");
    for c in calls {
        let _ = std::fs::remove_dir_all(store("held", c));
        let _ = std::fs::remove_dir_all(store("held1", c));
    }
}

#[test]
fn a_late_joiner_gets_the_object() {
    let calls = ["K1ABC", "W1AW", "W5LATE"];
    let data = announcement();
    let id = ObjectId::of(&data);
    let mut sim = net(&calls, 10.0, 17, |_| {});
    sim.on[3] = false;
    sim.stations[0].offer(&data);
    let mut ev = vec![Vec::new(); 4];
    let ok = run(&mut sim, &mut ev, 2400.0, |ev| {
        !objects(&ev[1]).is_empty() && !objects(&ev[2]).is_empty()
    });
    assert!(ok, "after {:.0} s: NCS {:#?}", sim.t, ev[0]);
    let before = sim.stations[0].stats.obj_pieces;
    sim.on[3] = true;
    let until = sim.t + 3000.0;
    let ok = run(&mut sim, &mut ev, until, |ev| !objects(&ev[3]).is_empty());
    assert!(ok, "after {:.0} s: W5LATE {:#?}", sim.t, ev[3]);
    assert_eq!(objects(&ev[3]), vec![(id, data.clone(), false)]);
    println!(
        "late joiner: {} pieces before, {} after",
        before, sim.stations[0].stats.obj_pieces
    );
    assert!(sim.stations[0].stats.obj_pieces > before);
    for e in &ev[1..3] {
        assert_eq!(objects(e).len(), 1);
    }
}

#[test]
fn a_store_copy_that_does_not_match_its_id_is_not_trusted() {
    let calls = ["K1ABC", "W1AW"];
    let data = announcement();
    let id = ObjectId::of(&data);
    let (mut sim, mut ev) = object_net("bad", &calls, &calls, &data, 18);
    // K1ABC's copy has gone bad: it must fetch it.
    let dir = store("bad", "K1ABC");
    let mut bad = data.clone();
    bad[100] ^= 1;
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(id.to_string()), &bad).unwrap();
    let ok = run(&mut sim, &mut ev, 2400.0, all_have_it);
    assert!(ok, "after {:.0} s: K1ABC {:#?}", sim.t, ev[1]);
    assert_eq!(objects(&ev[1]), vec![(id, data.clone(), false)]);
    assert_eq!(objects(&ev[2]), vec![(id, data.clone(), true)]);
    assert_eq!(std::fs::read(dir.join(id.to_string())).unwrap(), data);
    assert!(sim.stations[0].stats.obj_pieces > 0);
    for c in calls {
        let _ = std::fs::remove_dir_all(store("bad", c));
    }
}

#[test]
fn an_object_reaches_a_leaf_through_its_relay() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 8.0)], 19);
    let data = announcement();
    let id = ObjectId::of(&data);
    let mut ev = vec![Vec::new(); 4];
    assert!(run(&mut sim, &mut ev, 3000.0, |ev| joined(&ev[3])));
    sim.stations[0].offer(&data);
    let until = sim.t + 3600.0;
    let ok = run(&mut sim, &mut ev, until, all_have_it);
    assert!(
        ok,
        "after {:.0} s\nW5FAR {:#?}\nroster {:#?}",
        sim.t,
        ev[3],
        sim.stations[0].roster(sim.t)
    );
    assert_eq!(objects(&ev[3]), vec![(id, data, false)]);
    assert!(sim.stations[1].stats.obj_pieces > 0, "the relay sent it");
}

/// Close while a member is in the middle of its turn: NCS lets that turn
/// end (the burst under way acknowledged) before its Closes, so the member
/// is listening for them.
#[test]
fn a_close_waits_for_the_answer_under_way() {
    let calls = ["K1ABC", "W1AW"];
    let mut sim = net(&calls, 10.0, 21, |_| {});
    let mut ev = vec![Vec::new(); 3];
    assert!(run(&mut sim, &mut ev, 900.0, |ev| ev[1..].iter().all(|e| joined(e))));
    let long: Vec<u8> = (0..1500u32).map(|i| b'a' + (i % 26) as u8).collect();
    sim.stations[1].send(&long);
    let queued = sim.stations[1].pending();
    // Its first burst acknowledged, more to come: the next one is on its way.
    let ok = sim.run_until(sim.t + 900.0, |s| s.stations[1].pending() < queued);
    assert!(ok, "K1ABC never got a burst through");
    let at_close = sim.stations[1].pending();
    assert!(at_close > 0);
    sim.stations[0].close();
    let mut ncs_closed = None;
    let until = sim.t + 120.0;
    while sim.t < until && !ev[1..].iter().all(|e| e.contains(&NetEvent::Closed)) {
        for (i, s) in sim.stations.iter_mut().enumerate() {
            ev[i].extend(s.take_events());
        }
        if ncs_closed.is_none() && ev[0].contains(&NetEvent::Closed) {
            ncs_closed = Some(sim.stations[1].pending());
        }
        sim.step();
    }
    assert!(
        ev[1..].iter().all(|e| e.contains(&NetEvent::Closed)),
        "after {:.0} s: {:#?}",
        sim.t,
        ev
    );
    let left = ncs_closed.expect("NCS never closed");
    assert!(
        left < at_close,
        "the burst under way was not acknowledged ({left} of {at_close} B left)"
    );
}

#[test]
fn a_leaf_hears_the_close_through_its_relay() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 8.0)], 22);
    let mut ev = vec![Vec::new(); 4];
    assert!(run(&mut sim, &mut ev, 3000.0, |ev| joined(&ev[3])));
    assert_eq!(sim.stations[3].relay_call().as_deref(), Some("W1AW"));
    sim.stations[0].close();
    let until = sim.t + 120.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        ev[1..].iter().all(|e| e.contains(&NetEvent::Closed))
    });
    assert!(ok, "after {:.0} s\nW5FAR {:#?}", sim.t, ev[3]);
    assert!(!ev.iter().flatten().any(|e| *e == NetEvent::Lost));
    assert!(!sim.stations[1].close_pending());
}

/// A member that hears nothing of NCS for `lost_after_s` gives the net up;
/// a quiet net (idle rounds, check-ins closed so no beacons) is not lost,
/// nor is a leaf that hears only its relay.
#[test]
fn a_member_that_hears_nothing_of_ncs_gives_the_net_up() {
    let calls = ["W1AW", "K1ABC", "W5FAR"];
    let mut sim = tree_net(&calls, &[(1, 8.0)], 23);
    let mut ev = vec![Vec::new(); 4];
    assert!(run(&mut sim, &mut ev, 3000.0, |ev| ev[1..].iter().all(|e| joined(e))));
    sim.stations[0].set_open(false);
    let until = sim.t + 2400.0;
    run(&mut sim, &mut ev, until, |_| false);
    assert!(
        !ev.iter()
            .flatten()
            .any(|e| *e == NetEvent::Lost || *e == NetEvent::Closed),
        "{ev:#?}"
    );
    // NCS goes off the air.
    sim.on[0] = false;
    let off = sim.t;
    let until = off + 1200.0;
    let ok = run(&mut sim, &mut ev, until, |ev| {
        ev[1..].iter().all(|e| e.contains(&NetEvent::Lost))
    });
    assert!(ok, "after {:.0} s: {:#?}", sim.t - off, &ev[1..]);
    let lost_after = NetConfig::new(NCS, NET, false).lost_after_s;
    // Counted from the last burst heard, a round or so before NCS went.
    assert!(sim.t - off >= lost_after - 60.0, "lost after only {:.0} s", sim.t - off);
    assert!(sim.stations[1..].iter().all(|s| s.closed()));
}

/// A net whose 8 offers are all held by both members, as the list goes.
fn full_offers(test: &str, seed: u64) -> (NetSim, Vec<Vec<NetEvent>>, Vec<ObjectId>) {
    let calls = ["K1ABC", "W1AW"];
    let mut sim = net(&calls, 10.0, seed, |c| {
        if !c.ncs {
            let dir = store(test, &c.call);
            let _ = std::fs::remove_dir_all(&dir);
            c.store = Some(dir);
        }
    });
    let mut ev = vec![Vec::new(); 3];
    assert!(run(&mut sim, &mut ev, 1200.0, |ev| ev[1..].iter().all(|e| joined(e))));
    let ids: Vec<ObjectId> = (0..8u8).map(|k| sim.stations[0].offer(&vec![b'a' + k; 300])).collect();
    assert_eq!(sim.stations[0].offers().map(|o| o.unwrap()).to_vec(), ids);
    let until = sim.t + 3600.0;
    let ok = sim.run_until(until, |s| s.stations[0].roster(s.t).iter().all(|r| r.holds == 0xFF));
    assert!(ok, "after {:.0} s: {:#?}", sim.t, sim.stations[0].roster(sim.t));
    for (k, s) in sim.stations.iter_mut().enumerate() {
        ev[k].extend(s.take_events());
    }
    (sim, ev, ids)
}

/// K1ABC misses the list that puts `data` where an object it holds was:
/// net control must not take its old report for the new object, and it
/// must still get the new one.
fn missed_list_check(test: &str, sim: &mut NetSim, ev: &mut [Vec<NetEvent>], data: &[u8]) {
    let id = ObjectId::of(data);
    sim.on[1] = false;
    assert_eq!(sim.stations[0].offer(data), id);
    let i = sim.stations[0].offers().iter().position(|o| *o == Some(id)).unwrap();
    assert_eq!(i, 0, "an index held before");
    let until = sim.t + 120.0;
    run(sim, ev, until, |_| false);
    // Its next report goes (something in it changed), still with the old
    // list's holdings.
    sim.stations[1].set_traffic(Traffic {
        items: 1,
        ..Traffic::default()
    });
    sim.on[1] = true;
    let until = sim.t + 2400.0;
    let mut got = false;
    while sim.t < until && !got {
        for (k, s) in sim.stations.iter_mut().enumerate() {
            ev[k].extend(s.take_events());
        }
        got = objects(&ev[1]).iter().any(|o| o.0 == id);
        let r = sim.stations[0].roster(sim.t);
        let claimed = r.iter().any(|e| e.call == "K1ABC" && e.holds & 1 << i != 0);
        assert!(
            got || !claimed,
            "{test}: net control takes K1ABC to hold it before it has it"
        );
        for _ in 0..100 {
            sim.step();
        }
    }
    assert!(got, "{test}: K1ABC never got it: {:#?}", sim.stations[0].roster(sim.t));
    for c in ["K1ABC", "W1AW"] {
        let _ = std::fs::remove_dir_all(store(test, c));
    }
}

/// Re-offering (withdraw, then offer) with every other index in use puts
/// the new object on the old one's index.
#[test]
fn a_reoffer_on_a_used_index_reaches_a_member_that_missed_the_list() {
    let (mut sim, mut ev, ids) = full_offers("reoffer", 24);
    sim.stations[0].withdraw(ids[0]);
    missed_list_check("reoffer", &mut sim, &mut ev, b"the profile, edited");
}

/// A ninth offer (the summary at the close) takes the oldest one's index.
#[test]
fn a_ninth_offer_is_not_taken_as_held_by_a_member_that_held_the_oldest() {
    let (mut sim, mut ev, _) = full_offers("ninth", 25);
    missed_list_check("ninth", &mut sim, &mut ev, b"the summary");
}
