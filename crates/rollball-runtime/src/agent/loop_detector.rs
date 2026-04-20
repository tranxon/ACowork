//! Loop detection (Exact Repeat / Ping-Pong / No Progress)
//!
//! Three detection modes + three-level progressive response.
//! Adapted from ZeroClaw agent/loop_detector.rs
//! Rollball deviation: uses rollball-core ToolCall types; result includes
//! recommended action level.

use std::collections::HashMap;

/// Loop detection result
#[derive(Debug, Clone)]
pub enum LoopDetectionResult {
    /// No loop detected
    NoLoop,
    /// Loop detected with a specific response level
    LoopDetected {
        /// Which pattern was detected
        pattern: LoopPattern,
        /// Response level (Warning / Block / Break)
        level: ResponseLevel,
        /// Count of consecutive hits
        count: u32,
        /// Human-readable message
        message: String,
    },
}

/// Loop pattern type
#[derive(Debug, Clone)]
pub enum LoopPattern {
    /// Same (tool_name, params) called consecutively
    ExactRepeat,
    /// Two tools alternating A→B→A→B
    PingPong,
    /// Same tool, different params, but same result hash
    NoProgress,
}

/// Progressive response level
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseLevel {
    /// First hit: inject warning, continue
    Warning,
    /// Second hit: block the tool call, continue
    Block,
    /// Third hit: break the loop, terminate iteration
    Break,
}

/// Configuration for loop detection thresholds
#[derive(Debug, Clone)]
pub struct LoopDetectionConfig {
    /// Exact Repeat: consecutive identical calls threshold
    pub exact_repeat_threshold: u32,
    /// Ping-Pong: alternating cycle threshold
    pub ping_pong_threshold: u32,
    /// No Progress: same result hash threshold
    pub no_progress_threshold: u32,
    /// Whether No Progress detection is enabled
    pub no_progress_enabled: bool,
}

impl Default for LoopDetectionConfig {
    fn default() -> Self {
        Self {
            exact_repeat_threshold: 3,
            ping_pong_threshold: 4,
            no_progress_threshold: 5,
            no_progress_enabled: true,
        }
    }
}

/// Loop detector
pub struct LoopDetector {
    config: LoopDetectionConfig,
    /// Track consecutive identical (tool_name, params) calls
    exact_repeat_state: ExactRepeatState,
    /// Track alternating A→B pattern
    ping_pong_state: PingPongState,
    /// Track same tool + same result hash
    no_progress_state: NoProgressState,
    /// Per-pattern hit count (for progressive response)
    hit_counts: HashMap<String, u32>,
}

#[derive(Default)]
struct ExactRepeatState {
    last_signature: Option<String>,
    count: u32,
}

#[derive(Default)]
struct PingPongState {
    history: Vec<String>, // last N tool names
}

#[derive(Default)]
struct NoProgressState {
    tool_name: Option<String>,
    result_hashes: Vec<u64>,
    consecutive_same: u32,
}

impl LoopDetector {
    /// Create new loop detector with configuration
    pub fn new(config: LoopDetectionConfig) -> Self {
        Self {
            config,
            exact_repeat_state: ExactRepeatState::default(),
            ping_pong_state: PingPongState::default(),
            no_progress_state: NoProgressState::default(),
            hit_counts: HashMap::new(),
        }
    }

    /// Create with default configuration
    pub fn with_defaults() -> Self {
        Self::new(LoopDetectionConfig::default())
    }

    /// Check the latest tool call for loop patterns
    /// Call this after step ⑥ (append to history)
    pub fn check(&mut self, tool_name: &str, params: &str, result_content: &str) -> LoopDetectionResult {
        // Check Exact Repeat
        if let Some(result) = self.check_exact_repeat(tool_name, params) {
            return result;
        }

        // Check Ping-Pong
        if let Some(result) = self.check_ping_pong(tool_name) {
            return result;
        }

        // Check No Progress
        if self.config.no_progress_enabled
            && let Some(result) = self.check_no_progress(tool_name, result_content) {
            return result;
        }

        // Reset hit counters if no loop detected (different tool used successfully)
        // Only reset for patterns not currently being tracked

        LoopDetectionResult::NoLoop
    }

    fn check_exact_repeat(&mut self, tool_name: &str, params: &str) -> Option<LoopDetectionResult> {
        let signature = format!("{tool_name}:{params}");

        if self.exact_repeat_state.last_signature.as_ref() == Some(&signature) {
            self.exact_repeat_state.count += 1;
        } else {
            self.exact_repeat_state.last_signature = Some(signature.clone());
            self.exact_repeat_state.count = 1;
        }

        if self.exact_repeat_state.count >= self.config.exact_repeat_threshold {
            let key = "exact_repeat";
            let hit_val = {
                let hit_count = self.hit_counts.entry(key.to_string()).or_insert(0);
                *hit_count += 1;
                *hit_count
            };

            let level = self.response_level(hit_val);
            let message = format!(
                "Detected repeated call to [{tool_name}] with same parameters ({hit_val} consecutive hits)"
            );

            // Do NOT reset count and signature — escalation should continue
            // on subsequent identical calls. Only increment hit_counts.

            return Some(LoopDetectionResult::LoopDetected {
                pattern: LoopPattern::ExactRepeat,
                level,
                count: self.exact_repeat_state.count,
                message,
            });
        }

        None
    }

    fn check_ping_pong(&mut self, tool_name: &str) -> Option<LoopDetectionResult> {
        self.ping_pong_state.history.push(tool_name.to_string());

        // Keep only last N entries (2 * threshold)
        let max_len = (self.config.ping_pong_threshold * 2) as usize;
        if self.ping_pong_state.history.len() > max_len {
            let drain_count = self.ping_pong_state.history.len() - max_len;
            self.ping_pong_state.history.drain(0..drain_count);
        }

        // Check for A→B→A→B pattern
        let history = &self.ping_pong_state.history;
        if history.len() >= 4 {
            let len = history.len();
            let a = &history[len - 4];
            let b = &history[len - 3];
            let c = &history[len - 2];
            let d = &history[len - 1];

            // A==C, B==D, A!=B
            if a == c && b == d && a != b {
                // Count complete cycles
                let cycles = self.count_ping_pong_cycles(history);
                if cycles >= self.config.ping_pong_threshold {
                    let key = "ping_pong";
                    let hit_val = {
                        let hit_count = self.hit_counts.entry(key.to_string()).or_insert(0);
                        *hit_count += 1;
                        *hit_count
                    };

                    let level = self.response_level(hit_val);
                    let message = format!(
                        "Detected ping-pong between [{a}] and [{b}] ({cycles} cycles)"
                    );

                    self.ping_pong_state.history.clear();

                    return Some(LoopDetectionResult::LoopDetected {
                        pattern: LoopPattern::PingPong,
                        level,
                        count: cycles,
                        message,
                    });
                }
            }
        }

        None
    }

    fn count_ping_pong_cycles(&self, history: &[String]) -> u32 {
        if history.len() < 2 {
            return 0;
        }

        let mut cycles = 0u32;
        let mut i = 0;
        while i + 1 < history.len() {
            if i + 2 < history.len() && history[i] == history[i + 2] {
                cycles += 1;
                i += 2;
            } else {
                break;
            }
        }
        cycles
    }

    fn check_no_progress(&mut self, tool_name: &str, result_content: &str) -> Option<LoopDetectionResult> {
        let result_hash = self.simple_hash(result_content);

        match &self.no_progress_state.tool_name {
            Some(name) if name == tool_name => {
                // Same tool — check if result hash is same
                if let Some(&last_hash) = self.no_progress_state.result_hashes.last() {
                    if last_hash == result_hash {
                        self.no_progress_state.consecutive_same += 1;
                    } else {
                        self.no_progress_state.consecutive_same = 0;
                    }
                }
                self.no_progress_state.result_hashes.push(result_hash);

                if self.no_progress_state.consecutive_same >= self.config.no_progress_threshold {
                    let key = "no_progress";
                    let hit_val = {
                        let hit_count = self.hit_counts.entry(key.to_string()).or_insert(0);
                        *hit_count += 1;
                        *hit_count
                    };

                    let level = self.response_level(hit_val);
                    let message = format!(
                        "Detected no progress: [{tool_name}] returns same result repeatedly ({hit_val} hits)"
                    );

                    self.no_progress_state = NoProgressState::default();

                    return Some(LoopDetectionResult::LoopDetected {
                        pattern: LoopPattern::NoProgress,
                        level,
                        count: self.no_progress_state.consecutive_same,
                        message,
                    });
                }
            }
            _ => {
                // Different tool — reset
                self.no_progress_state = NoProgressState {
                    tool_name: Some(tool_name.to_string()),
                    result_hashes: vec![result_hash],
                    consecutive_same: 0,
                };
            }
        }

        None
    }

    /// Determine response level based on hit count
    fn response_level(&self, hit_count: u32) -> ResponseLevel {
        match hit_count {
            1 => ResponseLevel::Warning,
            2 => ResponseLevel::Block,
            _ => ResponseLevel::Break,
        }
    }

    /// Simple hash for result content (FNV-like)
    fn simple_hash(&self, s: &str) -> u64 {
        // Use first 256 chars + length for hash
        let content = if s.len() > 256 { &s[..256] } else { s };
        let mut hash: u64 = 14695981039346656037; // FNV offset basis
        for byte in content.bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(1099511628211); // FNV prime
        }
        hash ^= s.len() as u64;
        hash
    }

    /// Reset all detection state (e.g., when conversation starts)
    pub fn reset(&mut self) {
        self.exact_repeat_state = ExactRepeatState::default();
        self.ping_pong_state = PingPongState::default();
        self.no_progress_state = NoProgressState::default();
        self.hit_counts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_repeat_warning() {
        let mut detector = LoopDetector::with_defaults();

        // 3 identical calls (threshold = 3)
        let result1 = detector.check("weather", "{city:Shanghai}", "Sunny");
        assert!(matches!(result1, LoopDetectionResult::NoLoop));

        let result2 = detector.check("weather", "{city:Shanghai}", "Sunny");
        assert!(matches!(result2, LoopDetectionResult::NoLoop));

        let result3 = detector.check("weather", "{city:Shanghai}", "Sunny");
        if let LoopDetectionResult::LoopDetected { level, pattern, .. } = &result3 {
            assert!(matches!(pattern, LoopPattern::ExactRepeat));
            assert_eq!(*level, ResponseLevel::Warning); // First hit = Warning
        } else {
            panic!("Expected LoopDetected, got {result3:?}");
        }
    }

    #[test]
    fn test_exact_repeat_escalation() {
        let mut detector = LoopDetector::with_defaults();

        // First detection
        for _ in 0..3 {
            detector.check("weather", "{city:Shanghai}", "Sunny");
        }
        // Reset state between detections (simulates different iterations)
        // Second detection
        for _ in 0..3 {
            detector.check("weather", "{city:Shanghai}", "Sunny");
        }
        let _result = detector.check("weather", "{city:Shanghai}", "Sunny");
        let _result = detector.check("weather", "{city:Shanghai}", "Sunny");
        let result = detector.check("weather", "{city:Shanghai}", "Sunny");
        if let LoopDetectionResult::LoopDetected { level, .. } = &result {
            assert_eq!(*level, ResponseLevel::Break); // Third hit = Break
        }
    }

    #[test]
    fn test_no_loop_different_tools() {
        let mut detector = LoopDetector::with_defaults();

        let result = detector.check("weather", "{city:Shanghai}", "Sunny");
        assert!(matches!(result, LoopDetectionResult::NoLoop));

        let result = detector.check("calculator", "{expr:2+2}", "4");
        assert!(matches!(result, LoopDetectionResult::NoLoop));
    }

    #[test]
    fn test_simple_hash_deterministic() {
        let detector = LoopDetector::with_defaults();
        let h1 = detector.simple_hash("hello world");
        let h2 = detector.simple_hash("hello world");
        assert_eq!(h1, h2);

        let h3 = detector.simple_hash("hello earth");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_reset() {
        let mut detector = LoopDetector::with_defaults();
        detector.check("weather", "{city:Shanghai}", "Sunny");
        detector.reset();
        // After reset, should not trigger
        let result = detector.check("weather", "{city:Shanghai}", "Sunny");
        assert!(matches!(result, LoopDetectionResult::NoLoop));
    }
}
