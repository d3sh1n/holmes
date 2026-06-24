use reedline::Suggestion;
fn main() {
    let _s = Suggestion {
        value: "foo".to_string(),
        description: None,
        style: None,
        extra: None,
        span: reedline::Span::new(0, 0),
        append_whitespace: true,
        match_indices: vec![],
        display_override: None,
    };
}
