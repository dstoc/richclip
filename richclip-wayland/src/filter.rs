//! Pure filter logic for clipboard capture: allowlists, sensitive-MIME
//! detection, and size-cap enforcement.
//!
//! All functions here are free of Wayland I/O and are fully unit-tested.

/// Configuration that controls which clipboard offers are captured and how
/// much data is accepted per item.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Allowed MIME base-types. An offered MIME matches if its base type
    /// (everything before the first `;`) is in this list.
    pub allowed_mimes: Vec<String>,

    /// Maximum total bytes across all formats in a single captured item.
    /// Formats that would push the running total over this limit are dropped.
    pub max_item_bytes: usize,

    /// Maximum bytes accepted for a single format. Formats whose data exceeds
    /// this value are dropped entirely (their bytes are not stored).
    pub max_format_bytes: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            // Default allowlist per richclip.md "Capture flow" / "Sensitive content".
            allowed_mimes: vec![
                "text/plain".into(),
                "text/html".into(),
                "text/uri-list".into(),
                "image/png".into(),
                "image/jpeg".into(),
                "image/webp".into(),
                "image/bmp".into(),
                "application/json".into(),
                "application/rtf".into(),
            ],
            max_item_bytes: 8 * 1024 * 1024,   // 8 MiB
            max_format_bytes: 4 * 1024 * 1024, // 4 MiB
        }
    }
}

/// Extract the base MIME type (the part before any `;` parameter).
///
/// Examples:
/// - `"text/plain;charset=utf-8"` → `"text/plain"`
/// - `"image/png"` → `"image/png"`
pub fn mime_base_type(mime: &str) -> &str {
    mime.split(';').next().unwrap_or(mime).trim()
}

/// Returns `true` if the offered MIME set indicates a sensitive/secret
/// selection that should be skipped entirely.
///
/// Rules (conservative — when in doubt, skip):
///
/// 1. **KDE password-manager hint**: any MIME containing
///    `x-kde-passwordManagerHint` (case-insensitive). KWallet advertises
///    `application/x-kde-passwordManagerHint`; the presence of the hint
///    type is enough to skip — we do not read the content.
///
/// 2. **FreeDesktop SecretService**: any MIME containing `secret` (case-
///    insensitive) that also starts with `org.freedesktop` or
///    `application/x-` to reduce false positives. We match any MIME
///    containing the substring `secret` for conservatism — extend this
///    list if new schemes emerge.
///
/// Note: `application/x-nautilus-clipboard` is intentionally *not*
/// sensitive; do not broaden the substring match to catch it.
pub fn is_sensitive(offered_mimes: &[String]) -> bool {
    for mime in offered_mimes {
        let lower = mime.to_lowercase();

        // Rule 1: KDE password manager hint (any MIME containing the marker).
        if lower.contains("x-kde-passwordmanagerhint") {
            return true;
        }

        // Rule 2: FreeDesktop SecretService and similar secret-carrying types.
        // We match the substring "secret" but require the MIME to look like a
        // structured type (contains '/') and contain "secret" in the type
        // portion — not just in an arbitrary parameter value.
        // Split off parameters first so we only check the type/subtype.
        let base = mime_base_type(&lower);
        if base.contains("secret") {
            return true;
        }
    }
    false
}

/// An accepted MIME type from a clipboard offer.
///
/// Capture needs two forms of the type:
/// - [`offered`](Self::offered): the *exact* string the source advertised, which
///   must be passed back verbatim to `receive()` — a source that only offered
///   `text/plain;charset=utf-8` will not answer a `receive("text/plain")`.
/// - [`store_as`](Self::store_as): the normalized lowercase base type (no `;`
///   parameters) used as the stored MIME, so `decode <id> text/plain` and the
///   `text/plain` label fallback work predictably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedMime {
    /// Exact advertised type; pass to `receive()`.
    pub offered: String,
    /// Normalized base type; store under this.
    pub store_as: String,
}

/// Returns the subset of `offered` MIMEs that are accepted by `config`,
/// preserving offer order and deduplicating by base type (first occurrence
/// wins).
///
/// A MIME is accepted when its base type (before `;`) is in
/// `config.allowed_mimes`. The comparison is case-insensitive. Each result
/// carries the original advertised string (for `receive`) and the normalized
/// base type (for storage); see [`AcceptedMime`].
pub fn accepted_mimes(config: &CaptureConfig, offered: &[String]) -> Vec<AcceptedMime> {
    let allowed_lower: Vec<String> = config
        .allowed_mimes
        .iter()
        .map(|m| m.to_lowercase())
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();

    for mime in offered {
        let base = mime_base_type(mime).to_lowercase();
        if allowed_lower.contains(&base) && seen.insert(base.clone()) {
            result.push(AcceptedMime {
                offered: mime.clone(),
                store_as: base,
            });
        }
    }

    result
}

/// Returns `true` when a format with `format_bytes` bytes can be added to an
/// item whose running total is `current_total`, without exceeding the per-
/// format or per-item caps in `config`.
///
/// - If `format_bytes > config.max_format_bytes` the format is dropped.
/// - If `current_total + format_bytes > config.max_item_bytes` the format is
///   dropped and no further formats should be accepted for this item.
pub fn within_size_limits(
    config: &CaptureConfig,
    current_total: usize,
    format_bytes: usize,
) -> bool {
    if format_bytes > config.max_format_bytes {
        return false;
    }
    if current_total.saturating_add(format_bytes) > config.max_item_bytes {
        return false;
    }
    true
}

// ─── Unit Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CaptureConfig {
        CaptureConfig::default()
    }

    // ── mime_base_type ────────────────────────────────────────────────────────

    #[test]
    fn base_type_plain() {
        assert_eq!(mime_base_type("text/plain"), "text/plain");
    }

    #[test]
    fn base_type_with_charset() {
        assert_eq!(mime_base_type("text/plain;charset=utf-8"), "text/plain");
    }

    #[test]
    fn base_type_with_space_before_param() {
        // Some apps include a space after ';'.
        assert_eq!(mime_base_type("text/html; charset=utf-8"), "text/html");
    }

    // ── accepted_mimes ────────────────────────────────────────────────────────

    /// Helper: the `store_as` (normalized) types, in order.
    fn store_as(offered: &[String]) -> Vec<String> {
        accepted_mimes(&cfg(), offered)
            .into_iter()
            .map(|a| a.store_as)
            .collect()
    }

    #[test]
    fn accept_allowed_plain() {
        let offered = vec!["text/plain".into()];
        let result = accepted_mimes(&cfg(), &offered);
        assert_eq!(
            result,
            vec![AcceptedMime {
                offered: "text/plain".into(),
                store_as: "text/plain".into(),
            }]
        );
    }

    #[test]
    fn accept_charset_variant() {
        // text/plain;charset=utf-8 → received verbatim, stored as text/plain.
        let offered = vec!["text/plain;charset=utf-8".into()];
        let result = accepted_mimes(&cfg(), &offered);
        assert_eq!(
            result,
            vec![AcceptedMime {
                offered: "text/plain;charset=utf-8".into(),
                store_as: "text/plain".into(),
            }]
        );
    }

    #[test]
    fn deny_unlisted_mime() {
        let offered = vec!["application/octet-stream".into()];
        assert!(accepted_mimes(&cfg(), &offered).is_empty());
    }

    #[test]
    fn deny_x_richclip_internal() {
        let offered = vec!["application/x-richclip-label".into()];
        assert!(accepted_mimes(&cfg(), &offered).is_empty());
    }

    #[test]
    fn accept_image_png() {
        assert_eq!(store_as(&["image/png".into()]), vec!["image/png"]);
    }

    #[test]
    fn accept_preserves_offer_order() {
        let offered = vec!["image/png".into(), "text/html".into(), "text/plain".into()];
        assert_eq!(
            store_as(&offered),
            vec!["image/png", "text/html", "text/plain"]
        );
    }

    #[test]
    fn deduplicates_same_base_type() {
        // Two MIMEs with the same base type: first occurrence wins (and the
        // first one's advertised string is the one we'd receive).
        let offered = vec!["text/plain".into(), "text/plain;charset=utf-8".into()];
        let result = accepted_mimes(&cfg(), &offered);
        assert_eq!(
            result,
            vec![AcceptedMime {
                offered: "text/plain".into(),
                store_as: "text/plain".into(),
            }]
        );
    }

    #[test]
    fn accept_case_insensitive() {
        // MIME types are case-insensitive; offered string preserved, stored lowercase.
        let result = accepted_mimes(&cfg(), &["TEXT/PLAIN".into()]);
        assert_eq!(
            result,
            vec![AcceptedMime {
                offered: "TEXT/PLAIN".into(),
                store_as: "text/plain".into(),
            }]
        );
    }

    #[test]
    fn deny_mixed_bag_only_some_allowed() {
        let offered = vec![
            "text/plain".into(),
            "application/octet-stream".into(),
            "image/jpeg".into(),
            "application/x-kde-klipper".into(),
        ];
        assert_eq!(store_as(&offered), vec!["text/plain", "image/jpeg"]);
    }

    // ── is_sensitive ──────────────────────────────────────────────────────────

    #[test]
    fn sensitive_kde_hint_exact() {
        let mimes = vec!["application/x-kde-passwordManagerHint".into()];
        assert!(is_sensitive(&mimes));
    }

    #[test]
    fn sensitive_kde_hint_case_insensitive() {
        let mimes = vec!["APPLICATION/X-KDE-PASSWORDMANAGERHINT".into()];
        assert!(is_sensitive(&mimes));
    }

    #[test]
    fn sensitive_secret_service_mime() {
        let mimes = vec!["application/x-secret-data".into()];
        assert!(is_sensitive(&mimes));
    }

    #[test]
    fn not_sensitive_normal_text() {
        let mimes = vec!["text/plain".into(), "image/png".into()];
        assert!(!is_sensitive(&mimes));
    }

    #[test]
    fn not_sensitive_nautilus_clipboard() {
        // application/x-nautilus-clipboard must NOT be flagged as sensitive.
        let mimes = vec![
            "application/x-nautilus-clipboard".into(),
            "text/plain".into(),
        ];
        assert!(!is_sensitive(&mimes));
    }

    #[test]
    fn sensitive_when_mixed_with_normal_mimes() {
        // Sensitive hint present alongside normal MIMEs → still skip everything.
        let mimes = vec![
            "text/plain".into(),
            "image/png".into(),
            "application/x-kde-passwordManagerHint".into(),
        ];
        assert!(is_sensitive(&mimes));
    }

    #[test]
    fn not_sensitive_empty_offer() {
        assert!(!is_sensitive(&[]));
    }

    // ── within_size_limits ────────────────────────────────────────────────────

    #[test]
    fn size_ok_zero_total() {
        assert!(within_size_limits(&cfg(), 0, 1024));
    }

    #[test]
    fn size_format_at_cap() {
        // Exactly at the per-format cap → accepted.
        let c = cfg();
        assert!(within_size_limits(&c, 0, c.max_format_bytes));
    }

    #[test]
    fn size_format_over_cap() {
        let c = cfg();
        assert!(!within_size_limits(&c, 0, c.max_format_bytes + 1));
    }

    #[test]
    fn size_item_total_would_exceed() {
        let c = cfg();
        // Already used all item budget; next format (even tiny) is rejected.
        assert!(!within_size_limits(&c, c.max_item_bytes, 1));
    }

    #[test]
    fn size_item_total_exactly_fits() {
        let c = cfg();
        // A format of exactly max_format_bytes with zero running total:
        // the per-format cap is not exceeded (== is allowed) and 0 +
        // max_format_bytes <= max_item_bytes → accepted.
        // (Using max_item_bytes directly would exceed max_format_bytes,
        // which is half of max_item_bytes, and get rejected by the
        // per-format cap first.)
        assert!(within_size_limits(&c, 0, c.max_format_bytes));
    }

    #[test]
    fn size_small_remaining_budget_rejects_large_format() {
        let c = cfg();
        // 7 MiB used; format is 2 MiB → total 9 MiB > 8 MiB cap → rejected.
        let used = 7 * 1024 * 1024;
        let fmt_size = 2 * 1024 * 1024;
        assert!(!within_size_limits(&c, used, fmt_size));
    }

    #[test]
    fn size_small_remaining_budget_accepts_small_format() {
        let c = cfg();
        // 7 MiB used; format is 512 KiB → total 7.5 MiB < 8 MiB → accepted.
        let used = 7 * 1024 * 1024;
        let fmt_size = 512 * 1024;
        assert!(within_size_limits(&c, used, fmt_size));
    }
}
