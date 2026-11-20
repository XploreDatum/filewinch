# Releasing Filewinch

A release is a `v<version>` tag. GitHub Actions builds Windows and Linux and makes a **draft**
release; a Mac builds, signs and notarizes the macOS disk image and attaches it; then a person
reads the draft and publishes it.

| Platform | Built by | Files |
|---|---|---|
| macOS 13+ (universal) | `scripts/release-mac.sh` on a Mac | `Filewinch-<v>-macos-universal.dmg` |
| Windows x64 | `.github/workflows/release.yml` (`scripts/package-windows.ps1`) | `Filewinch-<v>-windows-x64.msi`, `.zip` |
| Linux x86_64, aarch64 | `.github/workflows/release.yml` (`scripts/package-linux.sh`) | `Filewinch-<v>-<arch>.AppImage`, `filewinch-<v>-linux-<arch>.tar.gz` |
| All | release workflow, then `release-mac.sh --upload` | `SHA256SUMS` (+ `SHA256SUMS.minisig` when minisign is set up) |

## Every release

1. Set the version in the root `Cargo.toml` (`[workspace.package] version`). It's compiled
   into every binary: the macOS Info.plist, the Windows version resource and the HTTP
   User-Agent.
2. Date the version's section in `CHANGELOG.md` (`## [0.9.0] - 2026-11-20`) and its
   `<release version="0.9.0" date="...">` in `packaging/linux/app.xpdt.filewinch.metainfo.xml`.
   The release workflow refuses a tag without both.
3. Commit, tag and push: `git tag v0.9.0 && git push origin v0.9.0`.
4. Wait for the **Release** workflow. It ends with a draft release holding the Windows and
   Linux files, `SHA256SUMS` and the release notes (from `scripts/release-notes.md.tmpl` and
   the CHANGELOG section).
5. On the Mac, from a clean checkout of the tag:
   `scripts/release-mac.sh 0.9.0 --upload`
   This builds both architectures, signs (hardened runtime, secure timestamp), notarizes and
   staples the app and the disk image, checks them with `codesign` and `spctl`, attaches the
   `.dmg` to the draft and adds its line to `SHA256SUMS`.
6. Open the draft on GitHub, read the notes, and publish it.

`scripts/release-mac.sh 0.9.0 --no-notarize --allow-dirty` is a dry run: it builds, signs and
verifies but doesn't notarize or upload.

## One-time setup

### macOS notarization profile

The Developer ID certificate ("Developer ID Application: XPLOREDATUM TECHNOLOGIES LLP
(SUHRWJYB95)") must be in the login keychain. Store an App Store Connect API key for
`notarytool` once:

```sh
xcrun notarytool store-credentials filewinch-notary \
  --key ~/.appstoreconnect/private_keys/AuthKey_<KEY ID>.p8 \
  --key-id <KEY ID> \
  --issuer <issuer UUID>
```

The key id is in the `.p8` file's name. The issuer UUID is in App Store Connect under
Users and Access > Integrations > App Store Connect API, above the keys table. The key needs the
Developer role. `release-mac.sh` stops with this command if the profile is missing.

### Windows code signing (optional)

Without it, Windows builds are unsigned and the release notes tell people how to get past
SmartScreen (More info, then Run anyway). To sign with
[Azure Trusted Signing](https://learn.microsoft.com/azure/trusted-signing/) (now "Artifact
Signing"), set these on the GitHub repo (Settings > Secrets and variables > Actions):

| Kind | Name | Value |
|---|---|---|
| Secret | `AZURE_TENANT_ID` | the Microsoft Entra tenant id |
| Secret | `AZURE_CLIENT_ID` | an app registration's client id |
| Secret | `AZURE_CLIENT_SECRET` | a client secret of that app registration |
| Variable | `AZURE_SIGNING_ENDPOINT` | the account's endpoint, e.g. `https://eus.codesigning.azure.net/` |
| Variable | `AZURE_SIGNING_ACCOUNT` | the Trusted Signing account name |
| Variable | `AZURE_SIGNING_PROFILE` | the certificate profile name |

The app registration needs the "Trusted Signing Certificate Profile Signer" role on the
account. With all six set, the workflow signs `Filewinch.exe` (before it's zipped and packed
into the MSI) and the MSI, and checks both signatures. With only some set, the run fails
instead of shipping unsigned by accident.

### minisign signature on SHA256SUMS (optional)

```sh
brew install minisign      # or: apt install minisign
minisign -G -p filewinch.pub -s filewinch.key            # asks for a password
```

Keep `filewinch.key` and its password somewhere safe and offline (a password manager). Then on
the GitHub repo set:

| Kind | Name | Value |
|---|---|---|
| Secret | `MINISIGN_SECRET_KEY` | the whole contents of `filewinch.key` |
| Secret | `MINISIGN_PASSWORD` | its password (leave unset for a key made with `-W`, no password) |
| Variable | `MINISIGN_PUBLIC_KEY` | the second line of `filewinch.pub` (starts with `RW`) |

The workflow then writes `SHA256SUMS.minisig`, and the notes show how to check it. Because
`release-mac.sh --upload` changes `SHA256SUMS`, it re-signs it when the release has a
signature; point it at the key: `FILEWINCH_MINISIGN_KEY=/path/to/filewinch.key`. It refuses to
upload rather than leave a signature that doesn't match.

## Building locally

Cargo writes to `<repo>/target` unless told otherwise. To build somewhere else on your machine
(e.g. an external drive), create `.cargo/config.toml`. It's gitignored, so it stays yours:

```toml
[build]
target-dir = "/Volumes/MyDrive/filewinch-target"
```

`CARGO_TARGET_DIR` works too and wins over the file.

## Updating Rune

Rune is a git dependency pinned to a `dev` revision in the root `Cargo.toml`. To update, pick
the new revision, compare Rune's root `Cargo.toml` `[patch.crates-io]` table at that revision
with ours, use the same `rev` for Rune and every patch, then run `cargo fetch` (it rewrites
`Cargo.lock` for the new revision) and the tests.
