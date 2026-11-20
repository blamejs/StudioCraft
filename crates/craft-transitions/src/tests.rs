use super::*;

const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

fn close(a: [f32; 4], b: [f32; 4]) -> bool {
    a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6)
}

#[test]
fn ids_names_and_labels_round_trip() {
    for k in Kind::ALL {
        assert_eq!(Kind::from_id(k.id()), Some(k));
        assert!(!k.name().is_empty());
    }
    for e in Edge::ALL {
        assert_eq!(Edge::from_id(e.id()), Some(e));
    }
    for e in Ease::ALL {
        assert_eq!(Ease::from_id(e.id()), Some(e));
    }
    assert_eq!(Kind::ALL.map(Kind::id), ["cut", "fade", "fade_color", "swipe"]);
    for unknown in ["", "Fade", "fade ", "wipe", "../cut"] {
        assert_eq!(Kind::from_id(unknown), None, "{unknown:?}");
        assert_eq!(Edge::from_id(unknown), None, "{unknown:?}");
        assert_eq!(Ease::from_id(unknown), None, "{unknown:?}");
    }
    assert!(Kind::Cut.instant() && Kind::ALL.iter().filter(|k| k.instant()).count() == 1);
}

#[test]
fn uniforms_follow_the_params() {
    let p = Params { color: [255, 0, 51, 7], edge: Edge::Bottom, ..Params::default() };
    assert_eq!(Kind::FadeThroughColor.uniform(&p), [1.0, 0.0, 0.2, 1.0]);
    assert_eq!(Kind::Swipe.uniform(&p), [0.0, -1.0, 0.0, 0.0]);
    assert_eq!(Kind::Cut.uniform(&p), [0.0; 4]);
    assert_eq!(Kind::Fade.uniform(&p), [0.0; 4]);
    let steps = Edge::ALL.map(|e| Kind::Swipe.uniform(&Params { edge: e, ..p }));
    assert_eq!(steps, [[1.0, 0.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, -1.0, 0.0, 0.0]]);
}

#[test]
fn eases_take_each_kinds_default_and_cut_stays_linear() {
    let none = Params::default();
    assert_eq!(Kind::Fade.ease(&none), Easing::Linear);
    assert_eq!(Kind::FadeThroughColor.ease(&none), Easing::Linear);
    assert_eq!(Kind::Swipe.ease(&none), Ease::EaseInOut.easing());
    let quad_in = Params { ease: Some(Ease::EaseIn), ..none };
    assert_eq!(Kind::Fade.ease(&quad_in), Easing::QuadIn);
    assert_eq!(Kind::Swipe.ease(&quad_in), Easing::QuadIn);
    assert_eq!(Kind::Cut.ease(&quad_in), Easing::Linear);
    // Ease in and out is the smoothstep 3t² − 2t³.
    let e = Ease::EaseInOut.easing();
    for k in 0..=100 {
        let t = f64::from(k) / 100.0;
        assert!((e.apply(t) - t * t * (3.0 - 2.0 * t)).abs() < 1e-9, "{t}");
    }
}

#[test]
fn audio_gains_cross_fade_and_take_the_new_audio_for_odd_values() {
    assert_eq!(Kind::Cut.audio_gains(0.2), (0.0, 1.0));
    assert_eq!(Kind::Fade.audio_gains(0.25), (0.75, 0.25));
    assert_eq!(Kind::Swipe.audio_gains(-3.0), (1.0, 0.0));
    assert_eq!(Kind::FadeThroughColor.audio_gains(9.0), (0.0, 1.0));
    for odd in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(Kind::Fade.audio_gains(odd), (0.0, 1.0), "{odd}");
    }
}

#[test]
fn cpu_blends_follow_the_shaders() {
    let new = |_| BLUE;
    assert_eq!(Kind::Cut.blend(RED, BLUE, [0.5, 0.5], 0.0, [0.0; 4], new), BLUE);
    assert!(close(Kind::Fade.blend(RED, BLUE, [0.5, 0.5], 0.25, [0.0; 4], new), [0.75, 0.0, 0.25, 1.0]));
    let green = Kind::FadeThroughColor.uniform(&Params { color: [0, 255, 0, 0], ..Params::default() });
    assert!(close(Kind::FadeThroughColor.blend(RED, BLUE, [0.5, 0.5], 0.25, green, new), [0.5, 0.5, 0.0, 1.0]));
    assert!(close(Kind::FadeThroughColor.blend(RED, BLUE, [0.5, 0.5], 0.5, green, new), [0.0, 1.0, 0.0, 1.0]));
    assert!(close(Kind::FadeThroughColor.blend(RED, BLUE, [0.5, 0.5], 0.75, green, new), [0.0, 0.5, 0.5, 1.0]));
    // From the left, a quarter of the way in: the new scene covers the left quarter.
    let left = Kind::Swipe.uniform(&Params::default());
    assert_eq!(Kind::Swipe.blend(RED, BLUE, [0.2, 0.5], 0.25, left, new), BLUE);
    assert_eq!(Kind::Swipe.blend(RED, BLUE, [0.3, 0.5], 0.25, left, new), RED);
    // The new scene is sampled where it has moved to.
    let mut seen = None;
    Kind::Swipe.blend(RED, BLUE, [0.1, 0.4], 0.25, left, |q| {
        seen = Some(q);
        BLUE
    });
    let q = seen.unwrap();
    assert!((q[0] - 0.85).abs() < 1e-6 && (q[1] - 0.4).abs() < 1e-6, "{q:?}");
}

#[test]
fn cpu_blends_clamp_hostile_progress_and_points() {
    let new = |_| BLUE;
    let left = Kind::Swipe.uniform(&Params::default());
    for k in Kind::ALL {
        let p = k.uniform(&Params::default());
        for (t, want) in [(f32::NAN, 1.0), (f32::INFINITY, 1.0), (2.0, 1.0), (f32::NEG_INFINITY, 0.0), (-1.0, 0.0)] {
            let got = k.blend(RED, BLUE, [0.5, 0.5], t, p, new);
            assert_eq!(got, k.blend(RED, BLUE, [0.5, 0.5], want, p, new), "{} at {t}", k.id());
            assert!(got.iter().all(|v| v.is_finite()), "{} at {t}: {got:?}", k.id());
        }
    }
    for uv in [[f32::NAN, 0.5], [0.5, f32::INFINITY], [-0.6, 0.5], [1.6, 0.5]] {
        assert_eq!(Kind::Swipe.blend(RED, BLUE, uv, 0.5, left, new), RED, "{uv:?}");
    }
}

#[test]
fn params_check_refuses_unknown_versions() {
    assert_eq!(Params::default().check(), Ok(()));
    assert_eq!(Params { version: 0, ..Params::default() }.check(), Err(ParamsError::NoVersion));
    let newer = Params { version: PARAMS_VERSION + 1, ..Params::default() };
    assert_eq!(newer.check(), Err(ParamsError::Newer(PARAMS_VERSION + 1)));
    assert!(ParamsError::Newer(u32::MAX).to_string().contains(&u32::MAX.to_string()));
}

#[cfg(feature = "serde")]
#[test]
fn params_read_and_write_with_serde() {
    let p = Params { color: [1, 2, 3, 4], edge: Edge::Top, ease: Some(Ease::EaseInOut), ..Params::default() };
    let json = serde_json::to_value(p).unwrap();
    assert_eq!(json, serde_json::json!({"version": 1, "color": [1, 2, 3, 4], "edge": "top", "ease": "ease_in_out"}));
    assert_eq!(serde_json::from_value::<Params>(json).unwrap(), p);
    // Missing fields take the defaults; the version must be there.
    assert_eq!(serde_json::from_str::<Params>(r#"{"version": 1}"#).unwrap(), Params::default());
    assert!(serde_json::from_str::<Params>("{}").is_err());
    // Unknown ids and out-of-range values are errors, not defaults.
    for bad in [
        r#"{"version": 1, "edge": "diagonal"}"#,
        r#"{"version": 1, "ease": "wobble"}"#,
        r#"{"version": 1, "color": [256, 0, 0, 0]}"#,
        r#"{"version": 1, "color": [0, 0, 0]}"#,
        r#"{"version": -1}"#,
    ] {
        assert!(serde_json::from_str::<Params>(bad).is_err(), "{bad}");
    }
    // A newer file reads, and check says so.
    let newer: Params = serde_json::from_str(r#"{"version": 7, "extra": true}"#).unwrap();
    assert_eq!(newer.check(), Err(ParamsError::Newer(7)));
    assert_eq!(serde_json::to_value(Kind::FadeThroughColor).unwrap(), "fade_color");
    for k in Kind::ALL {
        assert_eq!(serde_json::to_value(k).unwrap(), k.id());
    }
}
