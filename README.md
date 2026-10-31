# cua-login

Fill Bitwarden logins into Chrome through [cua-driver](https://github.com/trycua/cua) without the agent ever seeing them.

## Use

```sh
cua-login unlock     # you, in your own terminal: master password → session key in Keychain
cua-login status
cua-login lock       # deletes the stored session key and locks bw
cua-login selftest   # end-to-end check in a throwaway Chrome, no vault needed
```

Agents use the MCP server (`cua-login mcp`, registered with `claude mcp add -s user cua-login -- cua-login mcp`):

- `list_logins {search?}`: id, name, websites and whether TOTP is set up. No usernames or passwords.
- `fill_login {item, pid, window_id, fields?}`: types into the active tab of that Chrome window and returns only which fields were filled.

## Checks before anything is typed

- The tab's host must equal, or be a subdomain of, a website saved on the item, over https unless the saved URL is http.
- cua-driver must not be recording.
- Only inputs in the main frame are used, and every field must be found before any value is read from the vault.

## Limits

- The Bitwarden session key is stored as a Keychain item created by this binary, so other programs get a Keychain prompt. Rebuilding the binary changes its identity, so `cua-login` itself prompts once after each rebuild.
- `~/.claude/hooks/block-vault.sh` blocks Bash commands that touch `bw`, the session key or Keychain secrets. It stops accidents and prompt injection, not a determined agent running as your user.
- Without remote debugging, a Chrome, Brave or Edge window is filled through the accessibility tree. The tab's URL is read from the browser over AppleScript (one Automation prompt), the URL is checked again before each value is typed, and a password field that turns out not to be masked is cleared again. Only fields in the top frame are used, matched by their accessible labels.
- When cua-driver is attached to your Chrome (`cua-driver serve --grant existing-profile`, then `browser_prepare` with `existing_profile`), its DevTools route is used instead.
