use crate::cfg_if;

cfg_if! {
    if #[cfg(test)] {
        fn emitted() -> bool {
            true
        }
    } else {
        fn emitted() -> bool {
            false
        }
    }
}

#[test]
fn cfg_if_emits_matching_branch() {
    assert!(emitted());
}
