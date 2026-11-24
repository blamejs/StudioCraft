use super::close;
use crate::*;

fn e(name: &str) -> Expression {
    Expression::new(ExpressionName::parse(name))
}

#[test]
fn names_parse_to_presets_or_custom_names() {
    for p in Preset::ALL {
        assert_eq!(ExpressionName::parse(p.name()), ExpressionName::Preset(p));
    }
    assert_eq!(ExpressionName::parse("Happy"), ExpressionName::Custom("Happy".into()));
    assert_eq!(Preset::Ou.group(), Some(Group::Mouth));
    assert_eq!(Preset::BlinkRight.group(), Some(Group::Blink));
    assert_eq!(Preset::LookLeft.group(), Some(Group::LookAt));
    assert_eq!(Preset::Happy.group(), None);
    assert_eq!(ExpressionName::Custom("aa2".into()).group(), None);
}

#[test]
fn block_stops_the_group_while_the_expression_is_on() {
    let mut happy = e("happy");
    happy.override_blink = Override::Block;
    let list = [happy, e("blink"), e("blinkLeft"), e("aa")];
    let mut w = [0.01, 1.0, 0.6, 0.8];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [0.01, 0.0, 0.0, 0.8]);
    let mut w = [0.0, 1.0, 0.6, 0.8];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [0.0, 1.0, 0.6, 0.8]);
}

#[test]
fn blend_scales_the_group_down_by_the_sum() {
    let mut happy = e("happy");
    happy.override_mouth = Override::Blend;
    let mut sad = e("sad");
    sad.override_mouth = Override::Blend;
    let list = [happy, sad, e("aa"), e("oh"), e("blink")];
    let mut w = [0.25, 0.25, 0.8, 0.4, 1.0];
    apply_overrides(&list, &mut w);
    assert!(close(w[2], 0.4, 1e-6) && close(w[3], 0.2, 1e-6));
    assert_eq!(w[4], 1.0);
    let mut w = [0.75, 0.75, 0.8, 0.4, 1.0];
    apply_overrides(&list, &mut w);
    assert_eq!((w[2], w[3]), (0.0, 0.0));
}

#[test]
fn binary_expressions_are_all_or_nothing() {
    let mut happy = e("happy");
    happy.is_binary = true;
    happy.override_blink = Override::Blend;
    let mut blink = e("blink");
    blink.is_binary = true;
    let list = [happy, blink, e("blinkLeft")];
    // At 0.4 the binary happy is off, so it holds nothing back.
    let mut w = [0.4, 0.7, 0.5];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [0.0, 1.0, 0.5]);
    // At 0.6 it is fully on: blend at 1 stops the group.
    let mut w = [0.6, 0.7, 0.5];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [1.0, 0.0, 0.0]);
    // A binary target under any hold-back is off.
    let mut sad = e("sad");
    sad.override_blink = Override::Blend;
    let mut b = e("blink");
    b.is_binary = true;
    let mut w = [0.1, 1.0];
    apply_overrides(&[sad, b], &mut w);
    assert_eq!(w, [0.1, 0.0]);
}

#[test]
fn an_expression_does_not_hold_back_its_own_group() {
    let mut aa = e("aa");
    aa.override_mouth = Override::Block;
    let list = [aa, e("oh")];
    let mut w = [1.0, 0.5];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [1.0, 0.5]);
}

#[test]
fn weights_are_clamped_and_short_lists_are_handled() {
    let list = [e("happy"), e("aa")];
    let mut w = [1.5, f32::NAN];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [1.0, 0.0]);
    let mut short = [2.0];
    apply_overrides(&list, &mut short);
    assert_eq!(short, [1.0]);
    apply_overrides(&[], &mut [0.5]);
}

#[test]
fn custom_expressions_hold_back_presets_and_are_not_held_back() {
    let mut smirk = e("smirk");
    smirk.override_mouth = Override::Block;
    let mut happy = e("happy");
    happy.override_mouth = Override::Block;
    let list = [smirk, e("aa"), e("talk"), happy];
    let mut w = [1.0, 0.9, 0.9, 0.0];
    apply_overrides(&list, &mut w);
    assert_eq!(w, [1.0, 0.0, 0.9, 0.0]);
}
