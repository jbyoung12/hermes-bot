/// Find the largest byte index <= `max` that is a valid UTF-8 char boundary.
/// Equivalent to the nightly `str::floor_char_boundary`.
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Install a panic hook that logs panics via tracing.
/// This ensures panics in spawned tokio tasks are visible
/// even if the JoinHandle is not awaited.
pub fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic".to_string()
        };
        tracing::error!("Panic at {}: {}", location, payload);
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_floor_char_boundary_within_ascii() {
        assert_eq!(floor_char_boundary("hello world", 5), 5);
    }

    #[test]
    fn test_floor_char_boundary_at_len() {
        assert_eq!(floor_char_boundary("hello", 10), 5);
    }

    #[test]
    fn test_floor_char_boundary_exact_len() {
        assert_eq!(floor_char_boundary("hello", 5), 5);
    }

    #[test]
    fn test_floor_char_boundary_zero() {
        assert_eq!(floor_char_boundary("hello", 0), 0);
    }

    #[test]
    fn test_floor_char_boundary_empty_string() {
        assert_eq!(floor_char_boundary("", 5), 0);
    }

    #[test]
    fn test_floor_char_boundary_multibyte_utf8() {
        // "café": c(1) a(1) f(1) é(2) = 5 bytes
        let s = "café";
        assert_eq!(s.len(), 5);
        // max=4 lands inside 'é' (byte 4 is second byte of é) → backs up to 3
        assert_eq!(floor_char_boundary(s, 4), 3);
        // max=3 is the start of 'é' → valid boundary
        assert_eq!(floor_char_boundary(s, 3), 3);
        // max=5 is full length
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn test_floor_char_boundary_emoji() {
        // 🎉 is 4 bytes in UTF-8
        let s = "a🎉b";
        // a(1) + 🎉(4) + b(1) = 6 bytes
        assert_eq!(s.len(), 6);
        // max=2 lands inside the emoji → backs up to 1 (after 'a')
        assert_eq!(floor_char_boundary(s, 2), 1);
        // max=5 lands at 'b'
        assert_eq!(floor_char_boundary(s, 5), 5);
    }
}
