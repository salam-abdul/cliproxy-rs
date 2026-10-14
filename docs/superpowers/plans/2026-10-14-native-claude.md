# Native Claude Companion Implementation Plan

> Execute inline. No delegation is authorized. Keep each step independently testable.

**Goal:** finish sign-in, account toggles, settings and trustworthy health using the fork's existing manager plus transparent native transport.

**Architecture:** Rust manager owns OAuth and credential files; Node companion owns unchanged inference forwarding and persistent selection. A private supervisor coordinates one owner. Native CLI and ACP share the launcher. No OmniRoute runtime dependency.

1. [x] Add read-only manager credential adapter and tests: manager authentication, disabled state, credential rotation, stable identity, private path validation, expiry, no refresh.
2. [x] Reuse qualified transport/core, remove legacy credential owner, require authenticated manager health, expose sanitized status/counters; extend local fake tests.
3. [x] Add `cliproxy-native` login/accounts/enable/disable/settings/status/dashboard commands through existing management API, validate settings and return truthful failure exit codes. Test with fake management.
4. [x] Add private staged config, supervisor ownership lock, certificate maintenance and launcher; preserve native permissions and Remote Control. Test lifecycle without real credentials.
5. [x] Build pinned fork, run affected upstream OAuth/management tests, run all companion tests. Start a credential-free staging instance and verify management/dashboard/status.
6. [x] Save usage guide, measured evidence, handoff and rotated-credential rollback instructions. Commit durable local work; preserve upstream separation.
7. [ ] After user quits Zed, verify old sessions drained, transfer sole ownership, prove real refresh and small cache/phone loop, then switch defaults and retire OmniRoute. Never activate on incomplete proof.
