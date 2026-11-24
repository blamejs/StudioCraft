use super::close;
use crate::*;

fn exprs(names: &[&str]) -> Vec<Expression> {
    names.iter().map(|n| Expression::new(ExpressionName::parse(n))).collect()
}

fn weights(pairs: &[(Blendshape, f32)]) -> Weights {
    let mut w = Weights::default();
    for (s, v) in pairs {
        w.set(*s, *v);
    }
    w
}

#[test]
fn the_standard_table_maps_each_shape_to_its_expression() {
    use Blendshape as B;
    let names = ["blinkLeft", "blinkRight", "aa", "ih", "ou", "ee", "oh", "happy", "sad", "angry", "surprised", "relaxed"];
    let e = exprs(&names);
    let bound = Mapping::standard().bind(&e).unwrap();
    let cases: [(&[(B, f32)], &str, f32); 13] = [
        (&[(B::EyeBlinkLeft, 0.7)], "blinkLeft", 0.7),
        (&[(B::EyeBlinkRight, 0.4)], "blinkRight", 0.4),
        (&[(B::JawOpen, 0.6)], "aa", 0.6),
        (&[(B::MouthStretchLeft, 0.2), (B::MouthStretchRight, 0.6)], "ih", 0.4),
        (&[(B::MouthPucker, 0.5)], "ou", 0.5),
        (&[(B::MouthUpperUpLeft, 1.0)], "ee", 0.5),
        (&[(B::MouthFunnel, 0.3)], "oh", 0.3),
        // The emotions start at 0.2: 0.6 is (0.6 − 0.2) / 0.8 = 0.5.
        (&[(B::MouthSmileLeft, 0.6), (B::MouthSmileRight, 0.6)], "happy", 0.5),
        (&[(B::MouthSmileLeft, 0.2), (B::MouthSmileRight, 0.2)], "happy", 0.0),
        (&[(B::MouthFrownLeft, 1.0), (B::MouthFrownRight, 1.0)], "sad", 1.0),
        (&[(B::BrowDownLeft, 0.6), (B::BrowDownRight, 0.6)], "angry", 0.5),
        (&[(B::EyeWideLeft, 0.6), (B::EyeWideRight, 0.6)], "surprised", 0.5),
        // No default rule drives relaxed.
        (&[(B::EyeSquintLeft, 1.0), (B::EyeSquintRight, 1.0)], "relaxed", 0.0),
    ];
    let mut out = vec![0.0; e.len()];
    for (input, name, want) in cases {
        bound.eval(&weights(input), &mut out);
        let i = names.iter().position(|n| *n == name).unwrap();
        assert!(close(out[i], want, 1e-6), "{name}: {} for {want}", out[i]);
        let others = out.iter().enumerate().filter(|(j, v)| *j != i && **v != 0.0).count();
        assert_eq!(others, 0, "{name} moved other expressions: {out:?}");
    }
    assert!(!bound.drives(names.iter().position(|n| *n == "relaxed").unwrap()));
}

#[test]
fn blink_follows_both_eyes_only_without_one_eye_blinks() {
    use Blendshape as B;
    let w = weights(&[(B::EyeBlinkLeft, 0.9), (B::EyeBlinkRight, 0.5)]);
    let alone = exprs(&["blink"]);
    let mut out = vec![0.0; 1];
    Mapping::standard().bind(&alone).unwrap().eval(&w, &mut out);
    assert_eq!(out[0], 0.5);
    let all = exprs(&["blink", "blinkLeft", "blinkRight"]);
    let mut out = vec![0.0; 3];
    Mapping::standard().bind(&all).unwrap().eval(&w, &mut out);
    assert_eq!(out, vec![0.0, 0.9, 0.5]);
}

#[test]
fn rules_for_missing_expressions_are_skipped_and_kept() {
    let e = exprs(&["aa"]);
    let mut m = Mapping::standard();
    m.rules.push(Rule::new(ExpressionName::parse("wink"), Formula::single(Blendshape::EyeBlinkLeft)));
    let bound = m.bind(&e).unwrap();
    assert_eq!(bound.len(), 1);
    assert!(bound.drives(0));
    assert!(m.rules.iter().any(|r| r.expression.as_str() == "wink"));
    // An output shorter than the expression list is filled as far as it goes.
    bound.eval(&weights(&[(Blendshape::JawOpen, 1.0)]), &mut []);
}

#[test]
fn the_last_rule_for_an_expression_wins() {
    let e = exprs(&["aa"]);
    let mut m = Mapping { rules: vec![Rule::new(ExpressionName::parse("aa"), Formula::single(Blendshape::JawOpen))] };
    m.rules.push(Rule::new(ExpressionName::parse("aa"), Formula::single(Blendshape::MouthFunnel)));
    let mut out = [0.0];
    m.bind(&e).unwrap().eval(&weights(&[(Blendshape::JawOpen, 1.0), (Blendshape::MouthFunnel, 0.25)]), &mut out);
    assert_eq!(out[0], 0.25);
}

#[test]
fn threshold_and_gain_shape_the_output() {
    let r = Rule { threshold: 0.5, gain: 2.0, ..Rule::new(ExpressionName::parse("aa"), Formula::single(Blendshape::JawOpen)) };
    assert_eq!(r.shape(0.5), 0.0);
    assert!(close(r.shape(0.6), 0.4, 1e-6));
    assert_eq!(r.shape(0.9), 1.0);
    let f = Formula::of(Combine::Max, &[Blendshape::JawOpen, Blendshape::MouthFunnel]);
    assert_eq!(f.eval(&weights(&[(Blendshape::JawOpen, 0.2), (Blendshape::MouthFunnel, 0.7)])), 0.7);
    let f = Formula::of(Combine::Min, &[Blendshape::JawOpen, Blendshape::MouthFunnel]);
    assert_eq!(f.eval(&weights(&[(Blendshape::JawOpen, 0.2), (Blendshape::MouthFunnel, 0.7)])), 0.2);
    assert_eq!(Formula::of(Combine::Average, &[]).eval(&Weights::default()), 0.0);
}

#[test]
fn bad_rules_are_refused() {
    let base = Rule::new(ExpressionName::parse("aa"), Formula::single(Blendshape::JawOpen));
    let bad = [
        Rule { gain: 4.5, ..base.clone() },
        Rule { gain: f32::NAN, ..base.clone() },
        Rule { threshold: 1.0, ..base.clone() },
        Rule { threshold: -0.1, ..base.clone() },
        Rule { formula: Formula::of(Combine::Average, &[]), ..base.clone() },
        Rule { formula: Formula::of(Combine::Average, &[Blendshape::JawOpen; 9]), ..base.clone() },
    ];
    for r in bad {
        let m = Mapping { rules: vec![r.clone()] };
        assert!(m.validate().is_err(), "{r:?}");
        assert!(m.bind(&exprs(&["aa"])).is_err());
    }
    let m = Mapping { rules: vec![base; MAX_RULES + 1] };
    assert!(matches!(m.validate(), Err(AvatarError::TooLarge { .. })));
}

#[test]
fn a_mapping_reads_back_what_it_writes() {
    let m = Mapping::standard();
    let text = serde_json::to_string(&m).unwrap();
    assert!(text.contains("\"expression\":\"blinkLeft\""));
    assert!(text.contains("\"shapes\":[\"eyeBlinkLeft\"]"));
    let back: Mapping = serde_json::from_str(&text).unwrap();
    assert_eq!(back, m);
    let unknown = r#"{"rules":[{"expression":"aa","formula":{"combine":"average","shapes":["jawOpened"]}}]}"#;
    assert!(serde_json::from_str::<Mapping>(unknown).is_err());
    let custom = r#"{"rules":[{"expression":"wink","formula":{"combine":"max","shapes":["eyeBlinkLeft"]}}]}"#;
    let m: Mapping = serde_json::from_str(custom).unwrap();
    assert_eq!((m.rules[0].threshold, m.rules[0].gain), (0.0, 1.0));
    assert_eq!(m.rules[0].expression, ExpressionName::Custom("wink".into()));
    assert!(serde_json::from_str::<Mapping>(r#"{"rules":[{"expression":"","formula":{"combine":"max","shapes":["jawOpen"]}}]}"#).is_err());
}

#[test]
fn oversized_lists_are_refused_while_they_are_read() {
    let shapes = |n: usize| vec!["\"jawOpen\""; n].join(",");
    let rule = |n: usize| format!(r#"{{"expression":"aa","formula":{{"combine":"max","shapes":[{}]}}}}"#, shapes(n));
    let eight: Mapping = serde_json::from_str(&format!(r#"{{"rules":[{}]}}"#, rule(MAX_FORMULA_SHAPES))).unwrap();
    assert_eq!(eight.rules[0].formula.shapes.len(), MAX_FORMULA_SHAPES);
    let err = serde_json::from_str::<Mapping>(&format!(r#"{{"rules":[{}]}}"#, rule(MAX_FORMULA_SHAPES + 1))).unwrap_err();
    assert!(err.to_string().contains("more than 8 face shapes"), "{err}");
    let rules = |n: usize| vec![rule(1); n].join(",");
    assert_eq!(serde_json::from_str::<Mapping>(&format!(r#"{{"rules":[{}]}}"#, rules(MAX_RULES))).unwrap().rules.len(), MAX_RULES);
    let err = serde_json::from_str::<Mapping>(&format!(r#"{{"rules":[{}]}}"#, rules(MAX_RULES + 1))).unwrap_err();
    assert!(err.to_string().contains("more than 256 mapping rules"), "{err}");
}

#[test]
fn the_gaze_comes_from_both_eyes() {
    use Blendshape as B;
    // Looking to the subject's left: the left eye looks out and the right eye looks in.
    let (yaw, pitch) = gaze(&weights(&[(B::EyeLookOutLeft, 1.0), (B::EyeLookInRight, 1.0)]));
    assert_eq!((yaw, pitch), (GAZE_DEGREES, 0.0));
    let (yaw, _) = gaze(&weights(&[(B::EyeLookInLeft, 0.5), (B::EyeLookOutRight, 0.5)]));
    assert_eq!(yaw, -0.5 * GAZE_DEGREES);
    let (_, pitch) = gaze(&weights(&[(B::EyeLookUpLeft, 1.0), (B::EyeLookUpRight, 1.0)]));
    assert_eq!(pitch, -GAZE_DEGREES);
    let (_, pitch) = gaze(&weights(&[(B::EyeLookDownLeft, 1.0)]));
    assert_eq!(pitch, 0.5 * GAZE_DEGREES);
}
