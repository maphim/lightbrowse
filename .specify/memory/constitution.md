# Lightbrowse Constitution

> AI-native browser cho agent (CLI/MCP/HTTP) — token-budget, CDP, profile thật.
> Nguồn: repo maphim/lightbrowse. Constitution = source of truth nghiệp vụ;
> mọi spec/task phải đối chiếu.

## Core Principles

### I. Token Budget là Tính cách Cốt lõi (NON-NEGOTIABLE)
Mọi response trả về agent PHẢI đi qua budget (mặc định 1000 tok, `--max-tokens 0`
để tắt) kèm `reduction{strategy, original_tokens, reduced_tokens, saved_tokens}`
**trung thực** — không bao giờ double-count (bài học 0.6.1: text trả về 2 lần bị
coi là gian lận token). Số đo phải reproduce được.

### II. Browser Lifecycle — Graceful trước, Kill sau (NON-NEGOTIABLE)
Profile Chrome là state người dùng thật (cookie đăng nhập). Quy tắc:
- Shutdown/idle-kill: thử **graceful trước** (CDP `Browser.close` → Chrome tự
  flush cookie/profile), chờ timeout ngắn, rồi mới terminate.
- **Không bao giờ SIGKILL đầu tiên** — kill cứng làm mất cookie chưa flush +
  crash profile (bug 2026-11: O365 enterprise bị MFA lặp vì cookie không lưu).
- Khi spawn lại sau hard-kill: dọn **stale `SingletonLock`** (PID chết) trước —
  nếu không Chrome fallback sang profile rỗng = "mất login" oan.
- Zombie detect TRƯỚC khi kill (đã có — đừng bỏ) và KHÔNG BAO GIỜ kill browser
  của user khi chạy `--cdp-url` (external mode).

### III. Profile & Cookie = Hợp đồng với Người Dữ Liệu
Đăng nhập (cookie/session) PHẢI sống sót qua restart. Test E2E bắt buộc cho mọi
thay đổi lifecycle: login 1 lần → restart browser → vẫn đăng nhập. Đặc biệt
O365/enterprise (MFA đắt đỏ — mỗi lần login lại là một lần MFA).

### IV. Honesty & Quyền riêng tư
- Accounting trung thực (I); log KHÔNG ghi nội dung cookie/token.
- Profile dir_permissions chặt (user-only) — profile chứa session thật.
- Không gửi dữ liệu profile ra ngoài ngoài đích user yêu cầu.

### V. Đơn giản hoá (YAGNI)
Feature mới phải giải quyết vấn đề đo được; không thêm abstraction trước khi
có bug/đo chứng minh cần.

## Sửa đổi
Sửa constitution qua commit ghi rõ lý do đo được (không sửa ngầm khi "fix cho nhanh").
