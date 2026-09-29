//! Synthetic source for the benchmarks: roughly code shaped, about 50 byte
//! lines, repeated until it reaches a size.

/// `target_bytes` or a line more of it. `comment` is the third line of the
/// seven-line pattern, so a benchmark can choose whether it is multi-byte.
pub fn synth(target_bytes: usize, comment: &str) -> String {
    let mut s = String::with_capacity(target_bytes + 128);
    let mut i = 0usize;
    while s.len() < target_bytes {
        match i % 7 {
            0 => s.push_str("fn handle_event(&mut self, ev: &Event) -> Result<()> {\n"),
            1 => s.push_str("    let span = self.tree.node_at(ev.offset)?;\n"),
            2 => s.push_str(comment),
            3 => s.push_str("    match span.kind() { Kind::Ident => self.bump(), _ => {} }\n"),
            4 => s.push('\n'),
            5 => s.push_str("    self.dirty.insert(span.range());\n"),
            _ => s.push_str("}\n"),
        }
        i += 1;
    }
    s
}
