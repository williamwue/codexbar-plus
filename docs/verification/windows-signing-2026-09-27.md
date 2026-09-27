# Optional Windows release signing verification

The packaging script accepts an external PFX, resolves its absolute path, escapes Windows
signing arguments, and rejects missing signing inputs when `-RequireSignature` is set.
Certificate files are ignored. Each packaging run owns a separate staging directory and
restores the caller's update URL, including after build or packaging failures.

Validation on Windows, 2026-09-27:

- `cargo test --workspace --locked`: 216 passed, one process-helper test intentionally ignored.
- `pwsh -NoProfile -File tools/test-package-windows.ps1`: passed input validation,
  signed/unsigned arguments, quotes and trailing backslashes, existing staging preservation,
  temporary staging cleanup, environment restoration, and build/pack failure handling.
- `tools/package-windows.ps1 -UpdateUrl https://github.com/williamwue/codexbar-plus
  -OutputDir <external evidence directory>`: real release build and Velopack 1.2.0 packaging
  succeeded, producing Setup, Portable, full update package, and release manifests.
- Certificate ignore rules checked with `git check-ignore` for both `.pfx` and `.p12` paths.

The real packaging run was unsigned. No release was uploaded. Actual certificate signing,
timestamp service acceptance, public certificate trust, and SmartScreen reputation have
not been verified. The script checks use fixture certificates and mocked build/pack commands;
they do not establish cryptographic signature validity.

Velopack's documented Windows PFX flow:
https://docs.velopack.io/packaging/signing
