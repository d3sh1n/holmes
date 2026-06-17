use holmes_core::types::Memory;
use crate::memory_layer::MemoryLayer;

pub struct Consolidator {
    threshold: f64,
}

impl Consolidator {
    pub fn new(threshold: f64) -> Self { Self { threshold } }

    pub fn find_candidates(&self, memories: &[Memory]) -> Vec<(usize, usize, f64)> {
        let mut candidates = Vec::new();
        for i in 0..memories.len() {
            for j in (i + 1)..memories.len() {
                let sim = MemoryLayer::similarity(&memories[i], &memories[j]);
                if sim >= self.threshold {
                    candidates.push((i, j, sim));
                }
            }
        }
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
        candidates
    }

    pub fn summarize(a: &Memory, b: &Memory) -> String {
        format!("合并记忆:\n记忆 A: {}\n记忆 B: {}\n共同标签: {:?}",
            a.content, b.content,
            a.tags.iter().filter(|t| b.tags.contains(t)).collect::<Vec<_>>())
    }
}
