# Tonans — licence tiers and pricing

**Sales are not enabled in this build.**

There is no purchase flow in Tonans, no licence key to enter, no activation server,
and no payment integration compiled into the executable. The Analyst and Commercial
tiers below are **proposed** terms, published so that the entitlement code in the
application can be read against something. You cannot buy them today, and nothing
in this build tries to sell them to you.

Every shipped build runs in the Evaluation tier. This is not a trial that expires:
there is no clock, no counter, and nothing that phones home. It simply is what the
build does.

---

## The tiers

| | **Evaluation** | **Analyst** *(proposed)* | **Commercial** *(proposed)* |
|---|---|---|---|
| **Price** | Free | **$199 one-time** | **$999 one-time** |
| **Seats** | — | **1** | **Up to 5** |
| Bundled demonstration project | Yes | Yes | Yes |
| Your own projects | 1 | Unlimited | Unlimited |
| Scenarios per project | 3 | Unlimited (to the engine's 32) | Unlimited (to the engine's 32) |
| Sensitivity cells per study | 25 | 256 | 256 |
| Results visible on screen | Yes | Yes | Yes |
| Export bundle (HTML, CSV, JSON, provenance) | Yes | Yes | Yes |
| **Client-facing report rights** | **No** | **No** | **Yes** |
| Batch comparison, provenance artifacts, onboarding | — | — | Yes |
| Updates included | — | 12 months | 12 months |

One-time means one-time. The version you buy keeps working after the update window
closes; you simply stop receiving new versions. There is no subscription and no
kill switch.

### What "client-facing report rights" means

It is a **licence term, not a technical control.** Under Evaluation and Analyst,
an exported report carries a visible notice saying it is for internal evaluation
or internal use. Tonans states the term; it does not police it. We are not going to
pretend a watermark is enforcement, and we are not going to cripple an export you
paid for.

Commercial adds the right to hand a Tonans report to a client as part of your own
work product — under your own professional judgement and your own liability, not
ours.

### Paid pilots and calibration

Optional pilot or calibration work is scoped and quoted separately. It never
implies validation, certification, or endorsement of a design, and it never makes
Tonans a stamped engineering deliverable.

---

## What every tier is subject to

Tonans produces **planning estimates for human review**. It performs deterministic
single-bus energy accounting. It is not an AC power-flow study, a protection or
interconnection analysis, a safety certification, or a guarantee of savings. No
tier changes that, and no amount of money buys a claim the model cannot support.

The engine's hard limits — interval counts, scenario counts, sensitivity cells,
file sizes — are safety bounds, not upsells. **An entitlement can only ever be more
restrictive than the compiled-in envelope, never more permissive.** A licence cannot
buy its way past a bound that exists to keep a result honest, and there is a test
in `tonans-core` that fails the build if a tier ever tries to.

## Refunds

If Tonans does not do what this document says it does, write to
`support@flintglade.com` and you will get your money back. That policy will apply
when sales are enabled. Today there is nothing to refund.

---

Flintglade — `support@flintglade.com`
