# Outbound mTLS — Client-Certificate Identity for Egress

> **Audience:** operators wiring ulnclaw behind an mTLS-fronted gateway (C2/metering
> control plane), contributors touching egress paths
> **Source files:** `src/outbound_tls.rs` (policy + probe), `src/mtls_cmd.rs`
> (`ulnclaw mtls …`), `src/provider/openai.rs`, `src/provider/anthropic.rs`,
> `src/secret_scope.rs` (deployment-env allowlist)
> **Related:** `docs/design/multiplexing-gateway.md` (secret scopes),
> `docs/zh/providers.md` / `docs/en/providers.md` (provider egress),
> `src/iron_proxy.rs` (sandbox egress proxy — the *inbound-to-sandbox* direction)
> **Hermes parity:** the parity baseline (hermes-agent v2026.8.3) has **no** outbound
> client-certificate support. This is a deliberate ulnclaw extension;
> `docs/*/hermes-parity.md` records it as such.

## 1. Problem

Some deployments do not let an agent talk to upstream providers directly. Traffic is
routed through a **control plane** ("C2") that authorises, meters and audits every
call — typically an HTTPS endpoint that demands **mutual TLS**: the server presents
its certificate *and* requires the caller to present a client certificate it can
verify against its own CA.

Without client-certificate support the handshake dies. The control plane can only
fall back to "no identity", which in practice means **metering is disabled** — that
is the reported blocker: *"C2 metering waits for ulnclaw to implement outbound
mTLS."*

Observed before this change: `grep -rniE "mtls|client_cert" src/` → **0 hits**.
Every HTTPS client ulnclaw builds (`reqwest::Client::builder()`, 50+ call sites,
plus the raw `rustls`/`tokio-rustls` paths in `src/irc.rs`,
`src/email_platform.rs`) used the default trust store with `with_no_client_auth()`.
So the answer to "does ulnclaw speak mTLS outbound?" was **no**.

## 2. Goals / non-goals

**Goals**

1. Present a client certificate (with private key) on outbound TLS for a
   configurable set of hosts.
2. Trust a private CA bundle for the control plane (with an option to drop the
   public roots entirely, i.e. pin-only trust).
3. **Fail closed**: a half-configured or unreadable identity is a hard error on
   the egress path — never a silent downgrade to unauthenticated TLS.
4. Never leak the identity: the certificate is only presented to hosts in scope.
5. Give operators *verifiable* evidence: `ulnclaw mtls status` (resolved policy,
   redacted) and `ulnclaw mtls probe <url>` (real handshake, tells you whether the
   server actually demanded the certificate and whether ours was accepted).
6. Make onboarding possible without hand-written openssl incantations:
   `ulnclaw mtls keygen` (self-signed, staging) and `ulnclaw mtls csr`
   (production: key + CSR for the control plane's CA to sign).

**Non-goals**

- No per-profile (multiplexed) identities: the mTLS identity is a *deployment*
  setting of the process. Deployments needing one identity per tenant run one
  process per tenant (same reasoning as the deployment-level listener settings in
  `src/secret_scope.rs`).
- No TLS interception/CA generation of our own for egress. `src/iron_proxy.rs`
  already mints a CA for the *sandbox → daemon* direction; that machinery is
  unrelated and untouched.
- No certificate *lifecycle* automation (rotation, ACME). Ops rotate files; the
  policy re-reads them on the next client build.

## 3. Configuration surface

Read through `config::get_env_value`, so both the process environment and
`<home>/.env` work:

| Variable | Required | Meaning |
| --- | --- | --- |
| `ULNCLAW_MTLS_CERT` | to enable | PEM path: leaf certificate, optionally with intermediates. May also contain the private key. |
| `ULNCLAW_MTLS_KEY` | conditionally | PEM path of the private key (PKCS#8, PKCS#1 or SEC1). Required when `ULNCLAW_MTLS_CERT` holds no key. |
| `ULNCLAW_MTLS_CA` | no | PEM bundle of extra trust anchors (private CA of the control plane). |
| `ULNCLAW_MTLS_TRUST_BUILTIN` | no | `1` (default) adds public roots; `0` trusts **only** `ULNCLAW_MTLS_CA` (pin-only). |
| `ULNCLAW_MTLS_HOSTS` | **yes when identity is set** | Comma-separated scope: exact hosts, `*.example.com` wildcards, or `*` for every host. |
| `ULNCLAW_MTLS_MIN_TLS` | no | `1.2` (default) or `1.3`. |

Fail-closed validation rules:

- cert without a usable key → `key-missing` error naming the exact fix;
- key without cert → `cert-missing`;
- unreadable file, or no `CERTIFICATE` block in the cert PEM → error;
- identity set but `ULNCLAW_MTLS_HOSTS` missing/empty → `hosts-unscoped` error.
  Presenting a client certificate to *any* host that asks for one is a credential
  leak vector, so "identity to everyone" must be written down explicitly as `*`;
- `ULNCLAW_MTLS_MIN_TLS` other than 1.2/1.3 → error (no silent default).

## 4. Host scoping

`HostScope` matches the request host (case-insensitive, trailing dot ignored,
`:port` stripped):

- `All` — `*`, matches everything.
- `Hosts(list)` — exact match, or `*.suffix` wildcard matching one-or-more labels.

A host outside the scope gets a **plain** client: no identity, no private CA
preload. Scope is decided per client *build*, and providers are built per provider
instance, so a provider pointed at `api.openai.com` while the control plane lives
at `gw.example.com` only ever sees the identity on the latter.

## 5. Where the policy is applied

`outbound_tls::apply_env(builder, host)` is the single entry point. Wired so far:

| Path | File | Why |
| --- | --- | --- |
| OpenAI-compatible chat (incl. OpenAI, DashScope, OpenRouter, Ollama, llama.cpp, custom) | `src/provider/openai.rs` | the metered LLM path |
| Anthropic Messages API | `src/provider/anthropic.rs` | same |
| Audio in/out (`stt`, `tts`) | `src/stt.rs`, `src/tts.rs` | same traffic class |

Everything else (messaging platforms, webhooks, MCP, browser CDP) still builds its
own client. They can adopt the helper one call site at a time —
`outbound_tls::apply_env(reqwest::Client::builder(), host)?` — which is why the
helper takes a builder and a host instead of hiding the client construction. Until
a site adopts it, that site simply ignores the policy; the policy is never
*silently* assumed to be in force, and `ulnclaw mtls status` reports the wired
list.

Raw-`rustls` paths use `OutboundTls::rustls_config()` (same identity, same CA,
same min version, `with_client_auth_cert`).

## 6. `ulnclaw mtls` CLI

```
ulnclaw mtls status [--json]      # resolved policy: enabled, paths, fingerprint, scope, min TLS
ulnclaw mtls probe <url> [--timeout N] [--json]
ulnclaw mtls keygen --cn NAME [--days N] [--san DNS[,DNS…]] [--out DIR] [--force]
ulnclaw mtls csr    --cn NAME [--san DNS[,DNS…]] [--out DIR] [--force]
```

`status` prints the leaf **SHA-256 fingerprint** (public data) and `cert-not-after`
(parsed with the system `openssl x509 -enddate`, omitted if openssl is missing);
it never prints key material. Invalid config exits non-zero with the exact
remediation text.

`probe` runs the handshake **twice** against the same endpoint:

| with identity | without identity | verdict |
| --- | --- | --- |
| OK | handshake **or** request refused | `enforced` — the endpoint verifies client certificates and accepts ours |
| OK | request served | `not-required` — the endpoint answered without a client certificate |
| fails | — | `rejected` — handshake failed with our identity (trust/chain/expiry), with the TLS error |

The anonymous arm does not stop at the handshake: it sends one `GET /` and records
what comes back (`AfterHandshake::Served(<status>)` or `Refused(<error>)`). TLS 1.3
lets a server complete the handshake and only verify the client certificate
afterwards, so a handshake-only probe would call an enforcing gateway
"not-required". A refused request, or a 400/403/495/496/497 answer, counts as
`enforced`; a served request counts as `not-required`. Every verdict carries a
`reason` string with the raw evidence so the operator does not have to trust a
one-word answer.

That two-arm design is what makes the probe evidence instead of a guess: a single
successful request cannot distinguish "mTLS accepted" from "mTLS not required".

`keygen`/`csr` shell out to the system `openssl` (same assumption as
`src/iron_proxy.rs`), default the output directory to `<ulnclaw home>/mtls`,
refuse to clobber existing material unless `--force` is given, and chmod the
private key `0600` (the CSR/certificate keep openssl's default permissions).

## 7. Verification

- Unit tests in `src/outbound_tls.rs`: scope matching (exact / wildcard / `*` /
  port / case / trailing dot), every fail-closed rule, PEM loading + fingerprint,
  min-TLS parsing, status JSON shape.
- `tests/outbound_mtls_test.rs`: a local `tokio-rustls` server that **requires** a
  client certificate is started on a loopback port; the client built through
  `apply_env` completes a request against it, the same client *without* the
  identity is rejected by the server, and `probe_with` classifies the endpoint as
  `enforced` (and a server that does not ask for a certificate as `not-required`).
  This proves the feature end to end inside `cargo test`, not just that a struct
  round-trips. The PKI in `tests/fixtures/mtls/` is throwaway test material.
- Manual, on a real endpoint: `ulnclaw mtls probe https://<control-plane>/health`.

## 8. Operations notes

- Rotation: replace the PEM files, then restart (or rebuild the provider via the
  gateway's config reload). The policy is re-read whenever a client is built; no
  long-lived cached TLS state.
- Expiry: `ulnclaw mtls status` prints `cert-not-after`; wire it into your
  monitoring like any other expiry check.
- Failure signature: with the identity rejected, the handshake error surfaces as a
  provider/egress error mentioning the certificate — it is *not* retried as an
  anonymous request, by design.