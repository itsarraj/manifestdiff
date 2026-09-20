# manifestdiff

Diffs two Kubernetes Deployment manifests and calls out the changes that
actually carry operational risk — a removed env var, a swapped image
tag, replicas scaled down, a dropped volume mount, a lost health probe
— instead of a plain `diff` on the YAML, which shows every label
reorder and comment edit with the same visual weight as the change that
would actually break the rollout.

## Usage

```bash
manifestdiff before.yaml after.yaml
manifestdiff before.yaml after.yaml --json
```

Exit code `0` if nothing risky or breaking was found, `1` if the worst
finding is `RISKY`, `2` if anything is `BREAKING` — gateable in CI (fail
the pipeline on `2`, just warn on `1`).

```
$ manifestdiff before.yaml after.yaml

BREAKING CHANGES:
  - container "api": env var "FEATURE_FLAGS_URL" was removed
  - container "api": volumeMount "tls-certs" was removed
  - container "api": readinessProbe was removed
  - container "log-shipper" was removed

RISKY CHANGES:
  - replicas reduced from 4 to 2
  - container "api": image tag changed from 1.4.2 to 1.5.0
  - container "api": env var "LOG_LEVEL" value changed

INFO:
  - container "api": env var "TRACE_ID_HEADER" was added
  - container "metrics-exporter" was added
```

## What it checks

Per container (matched by name between the two manifests):

- **env vars** — removed → `BREAKING`, value or `valueFrom` changed →
  `RISKY`, added → `INFO`.
- **image** — tag changed on the same repository → `RISKY` (named
  explicitly: `"image tag changed from 1.4.2 to 1.5.0"`); the
  repository itself changed → also `RISKY`, phrased as a full image
  swap rather than a tag bump.
- **volumeMounts** — removed → `BREAKING`, mount path changed →
  `RISKY`, added → `INFO`.
- **livenessProbe`/`readinessProbe`** — removed → `BREAKING`, present in
  both but changed in any way → `RISKY`, added → `INFO`. Probe bodies
  are compared as whole YAML values, so *any* field changing under
  `httpGet`/`exec`/`tcpSocket` (path, port, timing, method) counts as
  "changed" — it doesn't try to explain which sub-field moved.

At the Deployment level: **`replicas`** reduced → `RISKY`, reduced to
exactly `0` → `BREAKING` (worded as "this scales the deployment to
nothing," since that's a full outage, not a capacity tweak); a whole
**container added or removed** → `INFO`/`BREAKING` respectively; and
`apiVersion`/`kind` itself changing → `BREAKING`.

**Labels, annotations, `selector`, container/env/mount ordering, and
any field outside the slice of the schema above are never
compared** — changing them produces zero findings, on purpose. That's
the actual point of a semantic diff over this shape: label churn (a
`team` or `environment` tag, cosmetic reordering) is common on every
real deploy and shouldn't be conflated with the changes that can
actually take a service down.

## Status: built, and verified against a hand-built realistic before/after pair with a genuine mix of six change types plus a harmless-only control

- **19 unit tests** (`cargo test --lib`): image-reference parsing
  (bare tag, no tag, and — the actual reason it's not a naive `split(
  ':')` — a registry with an explicit port so `registry.example.com:
  5000/team/app` isn't mistaken for a `5000/team/app` tag); every
  check independently for both its flagged and its correctly-silent
  case (a removed env var flagged `BREAKING`, an added one only
  `INFO`, a changed value `RISKY` and never `BREAKING`; a removed
  volume mount `BREAKING`; a removed liveness probe `BREAKING` while a
  *changed* readiness probe is `RISKY`, not `BREAKING`; replicas
  reduced to a nonzero number `RISKY`, reduced to exactly `0`
  `BREAKING`, and *increased* replicas producing no replica-related
  finding at all); a whole container added (`INFO` only — confirmed via
  `worst_severity`, not just the individual finding) versus removed
  (`BREAKING`); an `apiVersion`/`kind` change flagged `BREAKING`; two
  identical manifests producing zero findings; and — the specific claim
  this tool exists to make true — **a label-only change (both
  `metadata.labels` and the pod template's labels) producing zero
  findings**.
- **Live-verified against a hand-built, realistic before/after
  `Deployment` pair** (a `checkout-api` service, run through the real
  built `manifestdiff` binary, not just the test suite) with six
  simultaneous real changes: `replicas` 4→2, the `api` container's
  image tag `1.4.2`→`1.5.0`, its `FEATURE_FLAGS_URL` env var removed
  entirely, its `LOG_LEVEL` value changed, its `tls-certs` volume mount
  removed, its `readinessProbe` removed, and its sibling `log-shipper`
  sidecar container removed while a new `metrics-exporter` container
  was added — **all nine resulting findings landed in the exact
  expected bucket** (4 `BREAKING`, 3 `RISKY`, 2 `INFO` — the two `INFO`
  findings are the newly added `TRACE_ID_HEADER` env var and the newly
  added `metrics-exporter` container itself), with exit code `2`
  (breaking present). The unchanged `livenessProbe` on that same container
  produced no finding, confirming the tool isn't just flagging "the
  probes block changed" wholesale.
- **A second, isolated live run** against a copy of the same "before"
  manifest with *only* its `team` and `environment` labels edited (no
  other change) confirmed the harmless case explicitly: **zero
  findings, exit code 0** — the label edit really did produce no
  output, not just a low-severity one.
- Both fixture files were scratch YAML, deleted after the run.

**Not done / deliberately deferred**: multi-document YAML files (one
`Deployment` per file only — a real GitOps repo often concatenates
several resources with `---`, which this doesn't split); other
workload kinds (`StatefulSet`, `DaemonSet`, `CronJob` have different
shapes around `replicas`/`volumeClaimTemplates`/`schedule` that aren't
modeled here — a kind change between two Deployments is flagged, but a
`Deployment` vs `StatefulSet` diff isn't understood beyond that one
top-level flag); `initContainers` (only the main `containers` list is
diffed); resource requests/limits, `securityContext`, and
`nodeSelector`/affinity changes (real operational levers, just not
implemented in this pass); and image digest pins (`image@sha256:...`)
— only the `name:tag` form is parsed, so a digest-only change is
currently seen as a full image-string change rather than a distinct
"pinned to a new digest" finding.
