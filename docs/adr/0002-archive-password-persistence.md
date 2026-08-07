# Archive passwords: working password persisted (encrypted), spray-book not

The tool has two distinct "remember a password" behaviours for locked archives,
and they persist differently on purpose. The **spray-book** — candidate
passwords tried across archives and seeded into recovery — lives only in memory
for the session and is never written to disk. The **working password** — the
confirmed password that unlocks a specific archive — *is* persisted, stored with
that archive's index entry and encrypted behind an app passphrase entered once
per session.

## Why

- **Never re-crack.** Recovering a password can cost real time; losing it on app
  close, forcing a re-crack of an archive already opened, is the outcome to
  avoid. That argues for persisting the working password.
- **But secrets at rest are a real cost.** A stored password is a secret in the
  index database, which may be copied or backed up. Storing it in the clear
  would leak every archive password to anyone who reads the DB.
- **The split resolves the tension.** Confirmed working passwords are worth
  keeping, so they persist — but encrypted, so a stolen index is not a password
  dump. Candidate guesses are cheap to reproduce and not worth the at-rest risk,
  so the spray-book stays in memory only.

## Consequences

- Reading a persisted working password requires the app passphrase once per
  session; without it, a previously-opened archive re-prompts.
- The store schema gains an encrypted per-archive password field; the app gains a
  once-per-session passphrase gate.
- This is a security-posture decision and is hard to reverse cleanly: changing it
  later means migrating or invalidating stored secrets.
