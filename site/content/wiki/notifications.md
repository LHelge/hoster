---
title: Mail notifications
category: Operations
---

hoster can mail you when something needs you: a failed deploy, a crash loop, a failing
certificate, an unhealthy service. It mails again when the problem is over. Mail goes to one
recipient through an HTTP mail API. Resend is the only provider for now
([ADR-0058](https://github.com/LHelge/hoster/blob/main/docs/adr/0058-email-notifications.md)).

Notifications are off until you set them up. Everything is configured in the admin UI under
**Settings → Notifications**, not in `hoster.toml`. Changes apply without a restart, usually
within a minute. A mail whose delivery is being retried (see
[[notifications#failed-deliveries|Failed deliveries]]) still goes out with the settings it
started with, so a change can take up to about five minutes to reach it.

# Limits

Before you rely on it, know what it does not do:

- **Nothing is sent when hoster or the host is down.** hoster sends the mail itself, so a dead
  VPS, a stopped service or a broken network sends nothing. You still need an external uptime
  check for that.
- **One recipient.** Use a mailing list or a forwarding address if several people should get
  the mail.
- **No SMTP yet.** Only HTTP mail APIs, and of those only Resend so far. There is no SMS or chat
  webhook either.
- **Best effort.** A mail that cannot be delivered within five minutes is dropped, not queued
  (see [[notifications#failed-deliveries|Failed deliveries]]). The Events page keeps the full history for 30 days.
- **A fixed set of events.** You cannot pick kinds one by one. The only choice is whether to be
  mailed about successes as well.

Mail contains event text: stack, service and host names, image references and error messages.
That text leaves the server through the mail provider. Secret values and secret events are never
mailed.

# Set up Resend

1. **Create an account** at [resend.com](https://resend.com).
2. **Verify a sender domain.** In Resend's *Domains* page, add a domain you control (a
   subdomain such as `mail.example.com` is fine) and create the DNS records Resend lists for it.
   Wait until Resend shows it as verified. Until a domain is verified, Resend only accepts its
   test sender (`onboarding@resend.dev`), and only to the address of the Resend account itself.
3. **Create an API key** in Resend's *API Keys* page. Give it *Sending access* only; it can be
   limited to the domain from step 2. Copy it: Resend shows it once.

# Configure hoster

Open **Settings → Notifications** in the admin UI. Fill in:

| Field | What to enter |
| --- | --- |
| **Provider** | Resend. |
| **API key** | The key from Resend. It is stored encrypted in the secret store and is write-only: hoster never shows it again. To keep the stored key, leave the field empty when you save. |
| **From address** | The sender, for example `hoster@mail.example.com`. It must be on the domain you verified at Resend. |
| **Recipient** | Where the mail goes. |
| **Enabled** | Switch on to send notifications. |
| **Also notify on successes** | Also mail every successful deploy and every issued or renewed certificate, not only recoveries. |

Then **Save**. Switching notifications on needs the API key, the from address and the recipient;
hoster refuses it with a message on the field that is missing. While notifications are switched
off you can save an incomplete configuration.

Mail subjects start with the instance name, for example `[srv-01] Deployment of blog failed: …`.
The name is the machine's hostname unless you set one under **Settings → General**. The button in
each mail links to the admin host.

When notifications are switched on, hoster starts from the newest event. Nothing that happened
before, or while they were switched off, is mailed.

**Remove configuration** at the bottom of the page deletes the settings and the API key, after a
confirmation. Storing, replacing or deleting the key is recorded on the Events page, for example
"Set the Resend API key for notifications"; the key itself never is.

## Send a test

**Send test** delivers a fixed test message and shows Resend's answer under the button. It:

- uses the **saved** configuration, not what is typed in the form, so save first;
- works while notifications are switched off, so you can test before you switch them on;
- needs a saved API key, from address and recipient, and sends nothing without them;
- can be used once every 10 seconds.

When the test fails, the answer says why in plain words, followed by what Resend replied:

| Answer | What to do |
| --- | --- |
| **Invalid key** | The key is wrong, was revoked or may not send mail. Create a new one and save it. |
| **Sender domain not verified** | The from address is not on a domain verified at Resend. Verify the domain, or change the from address. If you are still on Resend's test sender, it only mails the Resend account's own address. |
| **Quota used up** | The Resend account's daily or monthly sending quota is spent. Wait, or upgrade the plan. |
| **Rate limited** | Resend asked hoster to slow down. Try again shortly. |
| **Network** | hoster could not reach Resend. Check the VPS's outbound HTTPS and DNS. |

The test counts as a delivery: its result shows as the **Last delivery** at the top of the page.

# Which events send mail

| When | Events |
| --- | --- |
| **Problems** | A deployment failed or was rolled back; a service entered a crash loop; a service became unhealthy; a certificate failed; a host's DNS does not point at this server; hoster rolled itself back. |
| **Recoveries** | A crash loop ended; a service is healthy again; a certificate was issued or renewed for a host whose problem was mailed; a stack deployed successfully after its failed deploy was mailed. |
| **Always** | hoster was updated. |
| **Only with "Also notify on successes"** | Every successful deploy and every issued or renewed certificate. |
| **Never** | Deploys starting, container restarts, stacks applied or removed, secrets set or deleted, hoster starting, and delivery failures. A deploy you cancelled, or one that ended because you removed its stack, is not mailed either. |

A recovery is mailed only if its problem was. A crash loop that you never heard about (because
it happened before notifications were on, or its mail was dropped) does not send a "recovered"
mail when it ends.

# Repeats and the hourly cap

hoster sends one mail when a problem starts and one when it ends, not one per attempt:

- **Repeats are held back.** While a crash loop, health problem, certificate failure or DNS
  mismatch is open, the same problem again is silent. A certificate that retries every few
  minutes sends one mail.
- **A daily reminder.** If the problem is still open 24 hours after the last mail about it,
  hoster mails it again, marked "still failing". Reminders stop once the event behind them is
  older than the Events page keeps (30 days); the problem stays open until it recovers.
- **Deploys are not held back.** Every failed or rolled-back deployment is its own mail. The
  stack's next successful deploy is mailed as the recovery.
- **At most 20 mails in any hour.** Events beyond that are counted, not mailed. Once the hour
  has room again, one summary mail says how many more events there were and links to the
  Events page. The summary counts towards the 20. A restart resets the cap (see below).
- **Old events are never mailed.** An event that is more than an hour old when hoster gets to
  it, for example after hoster was stopped for a while, is skipped. It still ends an open
  problem, silently.
- **After a hoster restart**, a crash loop or health problem that persists is mailed again as
  new: hoster rebuilds those states when it starts. Certificate and DNS problems carry over.
  The hourly cap is kept in memory, so it starts afresh, and a summary that was still owed is
  never sent; the held-back events are on the Events page.
- **Removing a stack** ends its open problems without a mail, and a **successful deploy** of a
  stack ends its crash-loop and health problems: the containers they were about are gone.

A service whose health flaps sends a problem and a recovery mail per flap, until the hourly cap
holds it back.

# Failed deliveries

When Resend does not accept a mail, hoster:

- retries a temporary failure (network error, timeout, Resend unavailable, rate limited) with
  back-off for up to **five minutes**, then drops the mail;
- drops the mail at once on a permanent failure (invalid key, unverified sender, quota used up,
  any other refusal).

A retry reuses the same idempotency key, so it never sends the same mail twice. A mail that was
dropped is gone: it is not sent later, and a problem whose mail was dropped is mailed as new if
it happens again.

Every delivery, sent or failed, updates **Last delivery** on Settings → Notifications. A dropped
mail also records a warning on the **Events** page: "A notification mail could not be
delivered", with Resend's reason on its detail line and a link back to Settings →
Notifications. While Resend keeps failing, hoster records at most **one such event an hour**,
so a revoked key does not fill the log. The link under Last delivery opens the Events page at
the operator and system group, warnings and up, where these events are. Delivery failures never
send mail.

# Backups and rollback

The notification settings and the encrypted API key are in `hoster.db`, so a database backup
includes them and restoring it restores them. The key needs the secret-store key to decrypt, as
every secret does ([[backup]]).

Rolling hoster back past the release that added notifications drops their tables. Configure
them again after such a rollback.
