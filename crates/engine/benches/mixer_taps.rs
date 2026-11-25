//! The mixer thread's real tick (`MixState::tick`) for one 1024-frame block, with audio tracks off
//! and on. `mix_state_8_inputs_taps_off` has no tap set, so each input checks for a tap and has
//! none. `mix_state_8_inputs_9_taps` taps every input and the program. `mix_state_63_inputs_64_taps`
//! taps the most tracks a take records at once. In the tapped cases a reader thread drains every
//! tap while the tick runs, as the collect thread does, so the tick shares the taps' queues with
//! another core.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use criterion::{Criterion, criterion_group, criterion_main};
use studiocraft_engine::mixer::{MixInput, MixState, MixerCmd, TapSet};
use studiocraft_media::Tick;
use studiocraft_media::audio::BLOCK_SAMPLES;
use studiocraft_media::mix::Levels;
use studiocraft_media::tap::TapSlot;
use studiocraft_model::{MixTracks, MonitorMode, SourceId};
use studiocraft_plugin::AudioOut;

/// A mixer with `inputs` inputs, each fed one block before every tick, and a tap for every input
/// and the program when `taps` is set.
fn mixer(inputs: u64, taps: bool) -> (MixState, Vec<Arc<AudioOut>>, Vec<Arc<TapSlot>>) {
    let mut m = MixState::new(Arc::new(Levels::default()));
    let outs: Vec<Arc<AudioOut>> = (0..inputs).map(|_| Arc::new(AudioOut::default())).collect();
    let list = outs
        .iter()
        .enumerate()
        .map(|(i, a)| MixInput {
            source: SourceId(i as u64 + 1),
            audio: Arc::clone(a),
            gain: 0.8,
            muted: false,
            active: true,
            levels: Arc::new(Levels::default()),
            filters: Vec::new(),
            sync_offset_ms: 0,
            monitor: MonitorMode::Off,
            mix_tracks: MixTracks::ALL,
            balance: 0.0,
            mono: false,
        })
        .collect();
    m.apply(MixerCmd::Inputs(list));
    let mut slots = Vec::new();
    if taps {
        let program = Arc::new(TapSlot::new());
        let inputs: Vec<(SourceId, Arc<TapSlot>)> = (0..inputs).map(|i| (SourceId(i + 1), Arc::new(TapSlot::new()))).collect();
        slots = inputs.iter().map(|(_, s)| Arc::clone(s)).chain([Arc::clone(&program)]).collect();
        m.apply(MixerCmd::Taps { set: Some(Arc::new(TapSet { program: Some(program), inputs })), generation: 1 });
    }
    (m, outs, slots)
}

fn feed(outs: &[Arc<AudioOut>]) {
    for a in outs {
        let mut b = a.block();
        b.samples.clear();
        b.samples.resize(BLOCK_SAMPLES, 0.25);
        a.push(b);
    }
}

/// Bench `name`: the tick for `inputs` inputs, with a reader thread draining the taps when
/// `taps` is set.
fn tick(c: &mut Criterion, name: &str, inputs: u64, taps: bool) {
    let (mut m, outs, slots) = mixer(inputs, taps);
    let stop = Arc::new(AtomicBool::new(false));
    let reader = taps.then(|| {
        let (slots, stop) = (slots.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut took = false;
                for s in &slots {
                    // The block goes back to the tap's pool when it drops.
                    took |= s.pop().is_some();
                }
                if !took {
                    std::thread::yield_now();
                }
            }
        })
    });
    let mut n = 0i64;
    c.bench_function(name, |b| {
        b.iter(|| {
            feed(&outs);
            n += 1;
            m.tick(Tick(n));
        })
    });
    stop.store(true, Ordering::Relaxed);
    if let Some(r) = reader {
        let _ = r.join();
    }
}

fn mixer_taps(c: &mut Criterion) {
    tick(c, "mix_state_8_inputs_taps_off", 8, false);
    tick(c, "mix_state_8_inputs_9_taps", 8, true);
    tick(c, "mix_state_63_inputs_64_taps", 63, true);
}

criterion_group!(benches, mixer_taps);
criterion_main!(benches);
