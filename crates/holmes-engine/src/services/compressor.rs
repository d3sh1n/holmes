use holmes_core::config::CompressorConfig;

#[derive(Debug, Clone)]
pub struct ContextCompressor {
    config: CompressorConfig,
    compressions: u32,
}

impl ContextCompressor {
    pub fn new(config: CompressorConfig) -> Self {
        Self { config, compressions: 0 }
    }

    pub fn should_compress(&self, messages: &[String]) -> bool {
        let total_chars: usize = messages.iter().map(|m| m.len()).sum();
        let estimated_tokens = total_chars / 4;
        estimated_tokens > (self.config.context_limit as f64 * self.config.threshold) as usize
    }

    pub fn compress(&mut self, messages: &[String]) -> Vec<String> {
        self.compressions += 1;
        if messages.len() <= self.config.protected_head + 5 {
            return messages.to_vec();
        }
        let head: Vec<String> = messages[..self.config.protected_head].to_vec();
        let tail_start = messages.len().saturating_sub(10);
        let tail: Vec<String> = messages[tail_start..].to_vec();
        let middle_len = messages.len() - head.len() - tail.len();
        let summary = format!(
            "[上下文压缩 #{}] 中间 {} 条消息已压缩。保留了头 {} 条和尾 {} 条消息中的关键信息。",
            self.compressions, middle_len, head.len(), tail.len()
        );
        let mut result = head;
        result.push(summary);
        result.extend(tail);
        result
    }

    pub fn compressions(&self) -> u32 { self.compressions }
}
