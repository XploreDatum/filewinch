# Setting up cloud sign-in (Google Drive, Dropbox, OneDrive, Box)

Filewinch signs in to cloud drives through the browser with OAuth 2 (Authorization Code
flow with PKCE, S256) and a **loopback redirect**: it listens on `127.0.0.1` for a moment,
opens the provider's consent page in the default browser, and catches the redirect back.
No client secret or ID is compiled into Filewinch. Before a provider works, register
Filewinch with that provider once and paste the IDs into a config file.

> **Status:** the four backends are tested against mock servers shaped on each provider's
> documented API. They have **not** yet been run against the real services; that needs the
> client IDs below. Expect a short round of fixes on first real use.

## Where the IDs go

`~/Library/Application Support/Filewinch/oauth-clients.json` (copy
[`oauth-clients.example.json`](oauth-clients.example.json)):

```json
{
  "google":   { "client_id": "….apps.googleusercontent.com", "client_secret": "GOCSPX-…" },
  "dropbox":  { "client_id": "<app key>" },
  "onedrive": { "client_id": "<application (client) ID GUID>" },
  "box":      { "client_id": "<client ID>", "client_secret": "<client secret>" }
}
```

Leave out providers you don't use. If the file has no entry, Filewinch falls back to the
environment variables `FILEWINCH_GOOGLE_CLIENT_ID`, `FILEWINCH_GOOGLE_CLIENT_SECRET`,
`FILEWINCH_DROPBOX_CLIENT_ID`, `FILEWINCH_ONEDRIVE_CLIENT_ID`, `FILEWINCH_BOX_CLIENT_ID`,
`FILEWINCH_BOX_CLIENT_SECRET` (a secret in the environment also fills a file entry that lacks
one). With neither, the app says e.g. *"Google Drive isn't set up yet: add a client ID (see
docs/oauth-setup.md)"*.

| Provider | Client ID | Secret needed | Redirect URI(s) to register |
|---|---|---|---|
| Google Drive | yes | **yes** (Google requires it even for desktop apps) | none: Desktop-app clients accept any loopback port |
| Dropbox | App key | no (PKCE public client) | `http://127.0.0.1:47811/callback` … `http://127.0.0.1:47815/callback` |
| OneDrive | Application (client) ID | no (public client) | `http://localhost/callback` (Microsoft ignores the port for `localhost`) |
| Box | Client ID | **yes** | `http://127.0.0.1:47811/callback` … `http://127.0.0.1:47815/callback` |

**Why a fixed port range for Dropbox and Box.** Both compare the redirect URI exactly,
port included, so Filewinch can't pick a random free port for them. It tries
**47811, 47812, 47813, 47814, 47815** in order and uses the first free one; register all five.
If all five are busy the app says so. Google and Microsoft accept any loopback port, so
Filewinch uses a random free port there. The listener only accepts `GET /callback`, checks
the `state` value, and closes after one sign-in (5-minute timeout, cancellable).

A desktop app can't truly keep a "client secret" secret (anyone can read it out of the app
or this file). Google and Box know this for installed apps; PKCE is what actually protects
the code exchange.

---

## Google Drive

1. Open the [Google Cloud console](https://console.cloud.google.com/), create a project
   (e.g. "Filewinch").
2. **APIs & Services → Library**: enable **Google Drive API**.
3. **Google Auth Platform** (formerly "OAuth consent screen"):
   - *Branding*: app name "Filewinch", support email, and (for production) home page and
     privacy policy links.
   - *Audience*: **External**. While the app's publishing status is **Testing**, add your
     own Google account(s) under *Test users*: only they can sign in.
   - *Data access*: add the scope `https://www.googleapis.com/auth/drive`.
4. *Clients → Create client*: application type **Desktop app**. Copy the **Client ID** and
   **Client secret** into `"google"`.
5. No redirect URI to register: Desktop-app clients accept `http://127.0.0.1:<any port>`.

What to know:
- `…/auth/drive` is a **restricted** scope. In *Testing* status Google issues refresh tokens
  that **expire after 7 days**, and only listed test users can sign in. Publishing to
  everyone requires Google's verification for restricted scopes (including a third-party
  security assessment). The narrower `drive.file` scope avoids that but only sees files
  Filewinch itself created or opened, which doesn't suit a file browser.
- Filewinch asks for `access_type=offline` and `prompt=consent` so Google always returns a
  refresh token.
- Google's out-of-band (copy/paste code) flow is retired; the loopback flow used here is the
  supported one for desktop apps.

## Dropbox

1. Open the [Dropbox App Console](https://www.dropbox.com/developers/apps) → **Create app**.
2. Choose **Scoped access**, then **Full Dropbox** (a file manager needs the whole Dropbox;
   "App folder" would confine Filewinch to `/Apps/<name>`). Name it.
3. **Permissions** tab: tick `account_info.read`, `files.metadata.read`,
   `files.metadata.write`, `files.content.read`, `files.content.write`, then **Submit**.
   (Changing scopes later means every user has to sign in again.)
4. **Settings** tab:
   - *OAuth 2 → Redirect URIs*: add each of `http://127.0.0.1:47811/callback` through
     `http://127.0.0.1:47815/callback` (five entries).
   - *Allow public clients (Implicit Grant & PKCE)*: **Allow**.
   - Copy the **App key** into `"dropbox"` → `client_id`. The app secret is not needed.
5. New apps start in *Development* status, which caps how many accounts can link; the
   console shows the limit and the **Apply for production** button.

What to know: Filewinch asks for `token_access_type=offline` to get a long-lived refresh
token; access tokens last about four hours and are refreshed automatically. Dropbox refresh
tokens don't rotate.

## Microsoft OneDrive (personal and work/school accounts)

1. Open the [Microsoft Entra admin center](https://entra.microsoft.com/) (or Azure portal) →
   **Identity → Applications → App registrations → New registration**.
2. *Name*: Filewinch. *Supported account types*: **Accounts in any organizational directory
   (any Microsoft Entra ID tenant – Multitenant) and personal Microsoft accounts**. This is
   what makes the `common` endpoint Filewinch uses accept both kinds of account.
3. *Redirect URI*: platform **Public client/native (mobile & desktop)**, value
   `http://localhost/callback`. Microsoft ignores the port for `localhost` redirect URIs, so
   Filewinch's random port matches. (Filewinch listens on both `127.0.0.1` and `::1` for it.)
4. **API permissions → Add a permission → Microsoft Graph → Delegated**: `Files.ReadWrite.All`,
   `User.Read`, `offline_access`. Grant admin consent if you administer the tenant.
5. Copy the **Application (client) ID** into `"onedrive"` → `client_id`. No secret: this is a
   public client. "Allow public client flows" is **not** required for this flow.

What to know:
- Work/school tenants can block user consent to unverified multi-tenant apps; a tenant admin
  may need to consent once, and for wide distribution set up **publisher verification**.
- Microsoft sometimes returns a new refresh token on refresh; Filewinch saves it (see below).
- Microsoft's documentation has at times said the registration UI rejects `http://127.0.0.1`
  redirect URIs (only the manifest editor accepts them); that is why Filewinch uses
  `localhost` for Microsoft only. If your portal shows different rules, `localhost` still works.

## Box

1. Open the [Box Developer Console](https://app.box.com/developers/console) → **Create New
   App** (Platform App) → **Custom App** → authentication method **User Authentication
   (OAuth 2.0)**.
2. **Configuration** tab:
   - *OAuth 2.0 Redirect URIs*: add `http://127.0.0.1:47811/callback` through
     `http://127.0.0.1:47815/callback` (Box allows plain `http` only for loopback addresses).
   - *Application Scopes*: **Read all files and folders stored in Box** and **Write all files
     and folders stored in Box**. Box sets scopes on the app, not per request.
   - Copy **Client ID** and **Client Secret** into `"box"`.
3. In an enterprise Box account the admin may have to enable the custom app (Admin Console →
   Integrations / Platform apps) before others can sign in.

What to know:
- Box refresh tokens are **single-use**: every refresh returns a new one and retires the
  old one (60-day lifetime). Filewinch reports each new token to the app so it is saved to
  the Keychain immediately; a crash between refresh and save means signing in again.
- Box's public docs don't describe PKCE for this flow. Filewinch sends the PKCE parameters
  (they're ignored if unsupported) and the client secret, which Box requires.

---

## How tokens are handled

- The refresh token lives in the macOS Keychain (service `app.xpdt.filewinch`); the site file
  only holds `Auth::OAuth { refresh_token: SecretRef }`. Access tokens stay in memory per
  session, are refreshed a minute before expiry, and once more if the API answers 401.
- **Rotated refresh tokens** (Box always, Microsoft sometimes) are handed to the app through
  `ConnectorOptions::on_token_rotated`, which must write the new token to the Keychain under
  the same `SecretRef`. See `docs/core-requests.md` (D-1) for the contract change that would
  make this part of `Connector`.
- Nothing logs tokens; `Debug` output of `SignedIn` and `ClientConfig` redacts them.

## How each drive appears in Filewinch

| | Layout | Delete | Server-side copy | Large uploads |
|---|---|---|---|---|
| Google Drive | `/My Drive`, `/Shared drives/<name>`, `/Shared with me` (read-only) | **moves to Drive's trash** | yes | resumable session over 5 MiB |
| Dropbox | `/` is your Dropbox | **deleted (restorable from Dropbox's deleted files / version history)** | yes | upload session over 8 MiB |
| OneDrive | `/My files`, `/Shared with me` | **moves to the OneDrive recycle bin** | yes (async, monitored) | upload session over 4 MiB |
| Box | `/` is All Files (folder `0`) | **moves to Box's trash** | yes | chunked session over 20 MB (Box's minimum) |

Notes that apply to several drives:
- Downloads resume from an offset (HTTP `Range`); uploads restart from the beginning if
  interrupted (`resume_write` is off: provider upload sessions don't outlive the transfer).
  Dropping an unfinished upload cancels the provider session (Drive, OneDrive, Box); Dropbox
  has no cancel call, and its abandoned sessions expire on Dropbox's side.
- OneDrive and Box need the file's total size before a session upload starts. The transfer
  engine always passes it; a write of unknown size that grows past the one-request limit
  (4 MiB / 20 MB) fails with a clear "needs to know the file's size" error.
- Rate limits (HTTP 429, Drive's 403 `rateLimitExceeded`, Graph's `activityLimitReached`,
  Dropbox's `too_many_requests`) come back as `RateLimited` with the provider's
  `Retry-After`, which the engine waits out before retrying.
- Box writes carry the item's `If-Match` etag; if the file changed on Box since Filewinch
  listed it, the write is refused rather than overwriting someone else's change.
- OneDrive: the items directly inside "Shared with me" can't be renamed or deleted from
  Filewinch (they belong to someone else); their contents follow the sharer's permissions.
- Dropbox team spaces aren't shown as a separate root; team folders appear where the
  Dropbox website shows them.

Google Drive specifics:
- Drive allows several items with the same name in one folder. Filewinch shows them all:
  the oldest-created keeps its name, later ones get " (2)", " (3)" … before the extension
  (`report (2).pdf`), ordered by creation time then ID, so names are stable between listings.
  Uploading to such a name replaces the item that holds it.
- Google Docs, Sheets, Slides and Drawings are listed as `.docx`, `.xlsx`, `.pptx` and `.pdf`
  with unknown size, and download as an export in that format. They can't be uploaded back
  over (an upload with that name creates a normal file).
- "Shared with me" is read-only in Filewinch; copy items into My Drive to edit them.
- Forms, Sites, My Maps, Jamboards and shortcuts aren't listed (there's nothing to download).
  A `/` inside a Drive name is shown as `／`.
- Copying folders isn't a Drive feature, so Filewinch copies them item by item.
