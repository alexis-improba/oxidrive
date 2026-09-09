# Google credentials setup (OAuth)

Guide to creating a Google Cloud project and wiring oxidrive with `client_id` / `client_secret`.

## Step 1: Create a Google Cloud project

1. Open [Google Cloud Console](https://console.cloud.google.com/).
2. Project menu → **New project** (or select an existing project).
3. Give it a clear name (e.g. "oxidrive sync") and create the project.

## Step 2: Enable the Google Drive API

1. In the project, **APIs & Services** → **Library**.
2. Search for **Google Drive API**.
3. Click **Enable**.

Without this step, oxidrive requests will fail on Google's side.

## Step 3: OAuth consent screen

1. **APIs & Services** → **OAuth consent screen**.
2. Choose **External** (or **Internal** if the Workspace account is limited to your organization).
3. Fill in the required fields (app name, support email, etc.).
4. In testing mode: add your account and other testers under **Test users**.

Until the app is verified for production, only test users can sign in.

## Step 4: Create OAuth 2.0 credentials (Desktop)

1. **APIs & Services** → **Credentials** → **Create credentials** → **OAuth client ID**.
2. Application type: **Desktop app** (or the equivalent "Desktop" option).
3. Name the client (e.g. "oxidrive desktop") and create.

Google displays the **Client ID** and **Client secret**.

## Step 5: Fill in `config.toml`

Copy `client_id` and `client_secret` into the OAuth section of your `config.toml`, using the keys expected by the project (exact names in the example shipped with oxidrive). Keep this file in the **parent** of the folder you will sync, not in the oxidrive git clone.

Do not commit this file if it contains secrets.

## Step 6: Finish with `oxidrive setup`

From that same parent directory (so oxidrive finds `./config.toml`):

```bash
oxidrive setup
```

Follow the browser flow: consent, then saving the token (often in `token.json` next to the config). When that is done, `sync` can use these credentials.

## Security

- **Never commit** `client_secret`, `token.json`, or a config that contains secrets to the repository.
- Add to **`.gitignore`** at least: `token.json`, any `config.local.toml` or other secret files, depending on your layout.
- If credentials leak: revoke the OAuth client in the console and create a new one.

For personal use, the **Desktop** type and test users are enough; publishing to production often involves a heavier Google review process.
