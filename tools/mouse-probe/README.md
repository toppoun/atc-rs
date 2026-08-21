# atc-rs pixel mouse probe

This is a deliberately isolated Phase-0 experiment for Windows 11 in the VS Code
Integrated Terminal. It reads terminal VT input directly and does not use the
production `atc-rs` crate or crossterm mouse events.

From the repository root in PowerShell:

```powershell
cargo test --manifest-path .\tools\mouse-probe\Cargo.toml
cargo run --manifest-path .\tools\mouse-probe\Cargo.toml
```

During capture, hold the left mouse button and drag vertically very slowly for
less than one full text row. Release it, then press `q`. The probe prints its
statistics and representative raw SGR reports only after mouse reporting and
the original Windows console modes have been restored.

The probe emits `CSI ? 1002 h` and `CSI ? 1016 h`, then disables them in
reverse order on exit. It also makes optional `CSI 14 t` and `CSI 16 t`
metric queries. It does not enable mode 1003 and does not fall back to
cell-coordinate mouse input.
