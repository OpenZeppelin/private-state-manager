# Deploying GUARDIAN Server to AWS ECS

This guide covers the current AWS deployment for Guardian. The AWS stack now uses Amazon RDS for PostgreSQL and no longer supports the legacy ECS-hosted Postgres runtime.

The deployment surface supports two stage profiles:
- `DEPLOY_STAGE=dev` keeps the current low-cost, fixed-capacity behavior
- `DEPLOY_STAGE=prod` enables ECS autoscaling, RDS storage autoscaling, RDS Proxy, larger default RDS sizing, RDS deletion protection with a final snapshot on destroy, and benchmark-oriented runtime defaults

## Published Docker images

Prebuilt, versioned server images are published to the GitHub Container Registry
(GHCR) at `ghcr.io/openzeppelin/guardian`, so you can pull a known-good image
instead of building from source:

```bash
docker pull ghcr.io/openzeppelin/guardian:<version>   # e.g. v1.2.3, or latest
```

Images are multi-architecture (`linux/amd64` + `linux/arm64`) and fully
runtime-configurable — every setting and secret is supplied at run time, never
baked in (see [`docs/CONFIGURATION.md`](./CONFIGURATION.md)):

```bash
docker run --rm -p 3000:3000 -p 50051:50051 \
  --env-file ./guardian.env \
  ghcr.io/openzeppelin/guardian:<version>
```

To run the published image with a Postgres backend locally, use the registry
compose file (no local build):

```bash
cp .env.registry.example .env.registry          # then set POSTGRES_PASSWORD in .env.registry
GUARDIAN_VERSION=<version> docker compose --env-file .env.registry -f docker-compose.registry.yml up
```

The stack is driven entirely by the gitignored `.env.registry` (see
`.env.registry.example`): Compose reads `POSTGRES_PASSWORD` / `GUARDIAN_VERSION` from
it for interpolation (via `--env-file`), and the server container loads it for
runtime config. The shared repo `.env` (AWS/deploy config) is intentionally not
used here, so this example never mutates it. The repo's default
`docker-compose.yml` (and the `docker-compose.postgres.yml` override) instead
build the server from source for contributors; `docker-compose.registry.yml` pulls
the published image.

Maintainers publish a version by running the **Docker Publish** GitHub Actions
workflow one of two ways:

- **On a GitHub Release.** Publishing a release auto-triggers the workflow: the
  version and build ref come from the release tag, the build uses the `postgres`
  feature, and an existing tag is never overwritten. The build first **waits for
  required-reviewer approval** on the `release` environment before it pushes —
  approve releases that should ship a server image, and decline ones that should
  not (e.g. SDK-only releases that share the same `vX.Y.Z` tag line). A release
  tag that does not match `vMAJOR.MINOR.PATCH[-prerelease]` fails the workflow.
  Cutting the release as a **draft** (`gh release create … --draft`) does not
  trigger the workflow — review it first, then publish the draft to start the build.
- **Manual dispatch.** Pick the branch/tag/commit to build from, the version to
  tag, the build features, and whether to overwrite an existing tag — for
  off-release or one-off builds.

In both cases a version containing `-` (e.g. `v1.2.3-rc.1`) is treated as a
pre-release and does not move the `latest` tag.

Published images are what the **AWS Deploy** workflow rolls out (next section).
`scripts/aws-deploy.sh` still builds and pushes its own image for infrastructure
changes and first-time stack setup.

## Deploying a published image from GitHub Actions

The **AWS Deploy** workflow (`.github/workflows/aws-deploy.yml`) deploys a
published GHCR version to the `stg` or `prod` stack without local AWS
credentials or Terraform state. Run it from the Actions tab with:

- `environment`: `stg` or `prod`
- `version`: a published tag such as `v1.2.3`. `stg` also accepts pre-releases
  like `v1.2.3-rc.1`; `prod` refuses them.

What a run does:

1. Resolves the version tag to an immutable digest and verifies that digest
   against the SLSA provenance attestation signed by the Docker Publish
   workflow. For `prod` the attestation must also show the build ran from the
   `refs/tags/<version>` release tag, so a manually dispatched build of another
   branch cannot ship to prod under a release version. This happens before the
   environment's protection rules, so `prod` reviewers are only asked to approve
   an already-verified digest.
2. Authenticates to AWS with GitHub OIDC (bootstrap role, then role chaining to
   the deploy role) and checks the stack's ECR repository and ECS service exist.
3. Mirrors the verified digest into `<stack>-server` in ECR, tagged both
   `<version>` and `latest`. Moving `latest` keeps
   `scripts/aws-deploy.sh plan` / `deploy --skip-build` resolving the deployed
   image, so a later Terraform apply does not roll the service back.
4. Registers a new revision of the task definition the service is currently
   running, with only the image changed, and waits up to 20 minutes for the
   rollout to stabilize.

The run summary records the deployed image URI and the previous task-definition
ARN. There is no automatic rollback: if a rollout fails, the summary prints the
`aws ecs update-service` command to restore the previous revision, or re-run the
workflow with the previously deployed version.

The workflow only rolls out an image. When a release also changes the task
definition (new environment variables, secrets, or IAM grants in `infra/`),
apply that release's Terraform first with `scripts/aws-deploy.sh`, then deploy
the image. Stacks that override the default `<stack>-server` / `<stack>-cluster`
resource names in Terraform (or `ECR_REPO_NAME` in the script) are not
supported by the workflow. The `guardian-evm` stack is also out of scope: GHCR
release images are built with the `postgres` feature only.

One-time setup per target (infra):

- A GitHub environment named `stg` / `prod` with variables `AWS_REGION`,
  `ROLE_FOR_OIDC` (role trusted for GitHub OIDC), `ROLE_TO_ASSUME` (deploy role
  reached via role chaining), and `STACK_NAME` (`guardian` / `guardian-prod`).
  Add required reviewers on `prod`.
- The OIDC role's trust policy must accept this repository's environment
  subject claims (`repo:OpenZeppelin/guardian:environment:stg` / `:prod`).
- The deploy role needs ECR push/pull on `<stack>-server`
  (`ecr:GetAuthorizationToken`, `ecr:DescribeRepositories`,
  `ecr:BatchCheckLayerAvailability`, `ecr:BatchGetImage`,
  `ecr:InitiateLayerUpload`, `ecr:UploadLayerPart`, `ecr:CompleteLayerUpload`,
  `ecr:PutImage`), `ecs:DescribeServices`, `ecs:DescribeTaskDefinition`,
  `ecs:RegisterTaskDefinition`, `ecs:UpdateService`, and `iam:PassRole` on the
  stack's task and task-execution roles.
- The ECR repository must already exist; `scripts/aws-deploy.sh build` creates
  it on a new stack.

## Prerequisites

- [Terraform](https://developer.hashicorp.com/terraform/downloads) >= 1.0
- AWS CLI configured with permissions for ECS, ECR, ELB, EC2, IAM, CloudWatch, RDS, and Secrets Manager
- Docker installed locally
- `jq` installed locally when deploying with `GUARDIAN_SERVER_FEATURES=postgres,evm`

```bash
aws sts get-caller-identity
docker info
terraform version
```

## Quick Start

```bash
aws sso login --profile <your-profile>

set -a && source .env && set +a

# Optional: build/deploy ARM64 instead of X86_64
# export CPU_ARCHITECTURE=ARM64

# Miden network the server runs against. The server requires this at startup;
# the deploy script passes MidenTestnet unless you override it here.
export GUARDIAN_NETWORK_TYPE=MidenTestnet

# Optional: allow dashboard operators and let Terraform create the secret
# export GUARDIAN_OPERATOR_PUBLIC_KEYS_JSON='["0x<alice-falcon-public-key>","0x<bob-falcon-public-key>"]'

# Optional: use an existing dashboard operator public keys secret instead
# export GUARDIAN_OPERATOR_PUBLIC_KEYS_SECRET_ARN='arn:aws:secretsmanager:us-east-1:123456789012:secret:guardian/operators'

# Optional: enable EVM support from config/evm/chains.json
# export GUARDIAN_SERVER_FEATURES=postgres,evm
# export GUARDIAN_EVM_CHAIN_CONFIG_FILE=config/evm/chains.json
# export GUARDIAN_CORS_ALLOWED_ORIGINS=https://accounts.openzeppelin.com

# Optional: choose the deployment profile
export DEPLOY_STAGE=dev
# export DEPLOY_STAGE=prod

# Optional: override the stack base name or public hostname
export STACK_NAME=guardian
# export SUBDOMAIN=guardian-stg

aws sts get-caller-identity
./scripts/aws-deploy.sh deploy
./scripts/aws-deploy.sh status
```

For a reviewable deployment, split image publishing, planning, and applying:

```bash
./scripts/aws-deploy.sh build
./scripts/aws-deploy.sh plan
./scripts/aws-deploy.sh deploy --skip-build
./scripts/aws-deploy.sh status
```

This builds and pushes `${ECR_REPO_NAME}:latest`, plans Terraform against the immutable digest currently behind that tag, then applies using the existing ECR image without rebuilding. If you push a new image after `plan`, rerun `plan` before `deploy --skip-build`.

## Terraform Variables

If you need to override defaults, use `infra/terraform.tfvars`:

```hcl
aws_region = "us-east-1"

# Optional: ECS/image architecture
# cpu_architecture = "X86_64"
# cpu_architecture = "ARM64"

# Optional: derive resource names from a base stack name
# stack_name = "guardian"

# Only set this when bypassing scripts/aws-deploy.sh. The deploy script resolves
# ECR latest to an immutable digest and passes server_image_uri via -var.
# server_image_uri = "123456789012.dkr.ecr.us-east-1.amazonaws.com/guardian-server@sha256:<digest>"

# Optional: Postgres credentials (defaults derive from stack_name)
# postgres_db       = "guardian"
# postgres_user     = "guardian"
# postgres_password = "guardian_dev_password"

# Optional: managed database sizing overrides
# Stage defaults:
# - dev  -> db.t3.micro, 20 GiB allocated, no storage autoscaling ceiling
# - prod -> db.t3.medium, 50 GiB allocated, 200 GiB max allocated
# rds_instance_class = "db.t3.medium"
# rds_allocated_storage = 50
# rds_max_allocated_storage = 200

# Optional: Miden network for the server runtime
# server_network_type = "MidenDevnet"

# Optional: dashboard operator Falcon public keys managed by Terraform
# guardian_operator_public_keys = [
#   "0x<alice-falcon-public-key>",
#   "0x<bob-falcon-public-key>",
# ]

# Optional: existing dashboard operator Falcon public keys secret
# guardian_operator_public_keys_secret_arn = "arn:aws:secretsmanager:us-east-1:123456789012:secret:guardian/operators"

# Optional: hosted ECDSA ACK signer backed by AWS KMS.
# Setting this is all that is required: Terraform grants the ECS task role
# kms:Sign + kms:GetPublicKey on the key and injects GUARDIAN_ACK_ECDSA_BACKEND
# and GUARDIAN_ACK_ECDSA_KMS_KEY_ID into the server runtime. The key must be
# ECC_SECG_P256K1 / SIGN_VERIFY. On this path the ECDSA Secrets Manager secret is
# not needed; the Falcon ACK secret bootstrap is unchanged and still required in
# prod.
# guardian_ack_ecdsa_kms_key_arn = "arn:aws:kms:us-east-1:123456789012:key/<key-id>"

# Optional: EVM runtime configuration
# guardian_evm_allowed_chain_ids = "1,11155111"
# guardian_evm_rpc_urls = "1=https://ethereum-rpc.publicnode.com,11155111=https://ethereum-sepolia-rpc.publicnode.com"
# guardian_evm_entrypoint_address = "0x433709009b8330fda32311df1c2afa402ed8d009"
# guardian_cors_allowed_origins = "https://accounts.openzeppelin.com"

# Optional: stage/runtime capacity overrides
# deployment_stage = "prod"
# server_desired_count = 2
# server_autoscaling_enabled = true
# server_autoscaling_min_capacity = 2
# server_autoscaling_max_capacity = 6
# server_autoscaling_cpu_target = 65
# server_autoscaling_memory_target = 75
# rds_proxy_enabled = true
# rds_proxy_subnet_ids = ["subnet-xxxxxxxx", "subnet-yyyyyyyy"]
# In us-east-1, avoid subnets in us-east-1e/use1-az3 for RDS Proxy.
# guardian_rate_limit_enabled = false
# guardian_rate_burst_per_sec = 200
# guardian_rate_per_min = 5000
# guardian_db_pool_max_size = 32
# guardian_metadata_db_pool_max_size = 32

# Optional: application metrics (ADOT sidecar + CloudWatch dashboard/alarms).
# Enabled by default; see "Metrics, Dashboard, And Alarms".
# guardian_metrics_enabled = true   # server Prometheus endpoint (loopback-only)
# cloudwatch_metrics_enabled = true # ADOT sidecar + dashboard + alarms
# metrics_namespace = "Guardian/Server"
# alarm_actions = ["arn:aws:sns:us-east-1:123456789012:guardian-alerts"]
# alarm_error_rate_threshold_percent = 5
# alarm_latency_threshold_seconds = 1
# alarm_cpu_threshold_percent = 85
# alarm_memory_threshold_percent = 90

# Optional: Route 53 hosted zone ID
# route53_zone_id = "Z1234567890ABC"

# Optional: Cloudflare DNS management
# cloudflare_zone_id = "..."
# cloudflare_api_token = "..."
```

## Database TLS verification

By default the server `DATABASE_URL` uses `sslmode=require` — the connection is
encrypted but the RDS certificate is **not verified**. To authenticate the
database, provide a CA bundle via Secrets Manager and set the
`rds_ca_bundle_secret_arn` Terraform variable.

> **Migrating an already-deployed stack?** Follow the staged, fail-safe procedure
> in [`runbooks/enable-db-tls.md`](./runbooks/enable-db-tls.md) (image-first,
> staging-before-prod, RDS-Proxy caveat, rollback). The rest of this section is
> the mechanism reference. The deployment then switches
`DATABASE_URL` to `sslmode=verify-full&sslrootcert=<mounted path>`, and both the
migration (libpq) and runtime (rustls) connections verify the certificate chain
and hostname.

**How delivery works (image stays CA-free).** The published image ships no CA
bundle. When `rds_ca_bundle_secret_arn` is set, Terraform adds a small
`rds-ca-initializer` **init container** to the task: it reads the secret, writes
it to a shared in-task volume, sets permissions, and exits; Fargate won't start
the Guardian container until it succeeds (`dependsOn: SUCCESS`). The Guardian
container mounts the same volume read-only and reads the bundle as a plain file.
The app never calls Secrets Manager and nothing is baked into the image.

**Combined CA bundle (required for RDS).** Production routes `DATABASE_URL`
through the **RDS Proxy** endpoint, which presents an AWS Certificate Manager
certificate that chains to **Amazon Trust Services** roots (specifically **Amazon
Root CA 1**) — *not* the Amazon RDS CA roots used by a direct instance. The secret
MUST therefore contain **both** root sets so `verify-full` succeeds against either
endpoint.

> **Size limit — do NOT use the global RDS bundle.** Secrets Manager caps a secret
> value at **64 KiB**. The `global-bundle.pem` (~165 KB) exceeds that and
> `create-secret` will reject it. Use your **region-specific** RDS bundle (a few
> KB) plus just **Amazon Root CA 1**, which keeps the combined bundle well under
> the cap. The Rust loader already supports multiple roots in one PEM.

Build it (mind the newline between files so the PEM blocks don't merge) and store
it verbatim — plain PEM text, no encoding:

```bash
# Region-specific RDS bundle (replace us-east-1), ~4-5 KB
curl -sS https://truststore.pki.rds.amazonaws.com/us-east-1/us-east-1-bundle.pem -o rds.pem
# All Amazon Trust Services roots (~6 KB total) — more rotation-tolerant than
# pinning only the one the RDS Proxy chains to today; still well under 64 KiB.
: > ats.pem
for ca in AmazonRootCA1 AmazonRootCA2 AmazonRootCA3 AmazonRootCA4 SFSRootCAG2; do
  curl -sS "https://www.amazontrust.com/repository/${ca}.pem" >> ats.pem; echo >> ats.pem
done
{ cat rds.pem; echo; cat ats.pem; } > rds-combined-ca.pem
grep -c "BEGIN CERTIFICATE" rds-combined-ca.pem   # sanity: total root count
test "$(wc -c < rds-combined-ca.pem)" -lt 65536 || echo "WARNING: bundle exceeds the 64 KiB Secrets Manager limit"

aws secretsmanager create-secret \
  --name guardian-prod/server/rds-ca-bundle \
  --secret-string file://rds-combined-ca.pem
```

> **Why Secrets Manager for a public cert?** CA roots aren't confidential, so this
> is a *consistency* choice — it reuses the same secret-injection plumbing and IAM
> pattern as `DATABASE_URL` and the ACK keys, with no new mechanism. The trade-offs
> are the 64 KiB cap above and ~$0.40/secret/month. If a future bundle must exceed
> 64 KiB, the next step is an S3 object (no size cap) or an EFS access point
> fetched by the same init container — the Rust/app side would not change.

Then set the ARN and apply:

```hcl
rds_ca_bundle_secret_arn = "arn:aws:secretsmanager:REGION:ACCOUNT:secret:guardian-prod/server/rds-ca-bundle-XXXXXX"
```

The execution role is granted `secretsmanager:GetSecretValue` on that ARN
automatically. If the bundle is missing or malformed, the init container or the
server preflight **fails closed** at startup rather than connecting insecurely.

**Rotation.** Update the secret's value (it may hold old and new roots together
for overlap), then force a new deployment so the init container re-reads the
secret and rewrites the file. Because the task definition still points at the
same secret ARN, changing only the secret value does **not** roll tasks on its
own — force it explicitly:

```bash
aws ecs update-service --cluster <cluster> --service <service> --force-new-deployment
```

No image or code change is required; ensure the new roots are present before they
become the only trusted ones.

## Deploy

### Script Commands

| Command | Purpose |
| --- | --- |
| `./scripts/aws-deploy.sh build` | Build the Guardian server image and push it to ECR as `latest`. Does not run Terraform. |
| `./scripts/aws-deploy.sh plan` | Run `terraform plan` using the immutable digest currently behind ECR `latest`. Does not build, push, or apply. |
| `./scripts/aws-deploy.sh deploy` | Build and push the image, resolve ECR `latest` to an immutable digest, and run `terraform apply`. |
| `./scripts/aws-deploy.sh deploy --skip-build` | Resolve the existing ECR `latest` image to an immutable digest and run `terraform apply` without rebuilding. |
| `./scripts/aws-deploy.sh bootstrap-ack-keys` | Create the prod ACK key secrets in Secrets Manager. Refuses to overwrite existing secrets. With `TF_VAR_guardian_ack_ecdsa_kms_key_arn` set, creates only the Falcon secret (ECDSA is KMS-backed). |
| `./scripts/aws-deploy.sh bootstrap-kms-ecdsa-key` | Create the KMS ECDSA ACK signing key (`ECC_SECG_P256K1` / `SIGN_VERIFY`) and an `alias/${STACK_NAME}-ack-ecdsa` alias, then print the ARN to set. Refuses to overwrite an existing alias. |
| `./scripts/aws-deploy.sh bootstrap-dashboard-cursor-secret` | Create the shared 32-byte dashboard cursor secret in Secrets Manager. Refuses to overwrite an existing secret. |
| `./scripts/aws-deploy.sh status` | Print Terraform outputs for the active `STACK_NAME` and `DEPLOY_STAGE`. |
| `./scripts/aws-deploy.sh logs` | Tail the deployed server's CloudWatch log group. |
| `./scripts/aws-deploy.sh cleanup` | Run Terraform destroy for the active `STACK_NAME` and `DEPLOY_STAGE`. |

`--skip-build` is meaningful for `deploy`; `plan` never builds or pushes an image. Use `build` before `plan` for a new stack or whenever ECR does not yet contain `${ECR_REPO_NAME}:latest`.

### One-Step Deploy

```bash
./scripts/aws-deploy.sh deploy
```

The deploy script resolves the ECR `latest` tag to an immutable digest before calling Terraform, so image pushes always produce a real ECS task-definition revision instead of relying on tag reuse.
It also keeps separate local Terraform state files per `STACK_NAME` and `DEPLOY_STAGE`, using `infra/terraform.<stack>.<stage>.tfstate` by default.

AWS deployments must include the `postgres` server feature. The script defaults `GUARDIAN_SERVER_FEATURES` to `postgres`; set `GUARDIAN_SERVER_FEATURES=postgres,evm` only when deploying the optional EVM API surface.

### Reviewable Build, Plan, Apply

Use this flow when you want to inspect Terraform changes before applying them:

```bash
./scripts/aws-deploy.sh build
./scripts/aws-deploy.sh plan
./scripts/aws-deploy.sh deploy --skip-build
```

`build` creates the ECR repository if needed and pushes `${ECR_REPO_NAME}:latest`. Both `plan` and `deploy --skip-build` resolve that tag to an immutable digest before invoking Terraform. Do not rebuild or push a new `latest` between `plan` and `deploy --skip-build` unless you intend to apply a different image; rerun `plan` after any rebuild.

For `DEPLOY_STAGE=prod`, bootstrap the ACK and dashboard cursor secrets once
before the first deploy:

```bash
DEPLOY_STAGE=prod ./scripts/aws-deploy.sh bootstrap-ack-keys
DEPLOY_STAGE=prod ./scripts/aws-deploy.sh bootstrap-dashboard-cursor-secret
```

The normal deploy path does not create or rotate these secrets. It expects the
prod Secrets Manager entries to already exist. Terraform injects the cursor
secret into every ECS task as `GUARDIAN_DASHBOARD_CURSOR_SECRET`.

Secret names default to `${STACK_NAME}/server/ack-{falcon,ecdsa}-secret-key`, so distinct stacks (e.g. `guardian-prod`, `guardian-prod-eu`) automatically resolve to distinct secrets and multiple Guardian deployments can coexist in the same AWS account. Override per stack by setting `GUARDIAN_ACK_FALCON_SECRET_NAME` / `GUARDIAN_ACK_ECDSA_SECRET_NAME` before `bootstrap-ack-keys` and `deploy`; they flow into Terraform variables and the ECS task definition's `GUARDIAN_ACK_FALCON_SECRET_ID` / `GUARDIAN_ACK_ECDSA_SECRET_ID` env vars.

The cursor secret defaults to
`${STACK_NAME}/server/dashboard-cursor-secret`. To use an existing secret, set
`GUARDIAN_DASHBOARD_CURSOR_SECRET_NAME` before both bootstrap and deploy.
The deploy helper always passes this resolved name explicitly, so a stale
`infra/terraform.tfvars` value cannot make validation and deployment select
different secrets. For a customer-managed KMS key, its key policy must also
allow the ECS task execution role to decrypt the secret.

#### Prod with a KMS-backed ECDSA signer

To keep the ECDSA private key in AWS KMS (never resident in the server process) while Falcon stays in Secrets Manager, create the key first and export its ARN before the rest of the flow. The script keys off `TF_VAR_guardian_ack_ecdsa_kms_key_arn` (the env var, not `terraform.tfvars`) to skip the ECDSA Secrets Manager secret, so it must be exported **before** `bootstrap-ack-keys` and `deploy`:

```bash
export DEPLOY_STAGE=prod STACK_NAME=<stack>

./scripts/aws-deploy.sh bootstrap-kms-ecdsa-key                  # creates the key, prints the ARN
export TF_VAR_guardian_ack_ecdsa_kms_key_arn="arn:aws:kms:...:key/<key-id>"
./scripts/aws-deploy.sh bootstrap-ack-keys                      # Falcon only; skips ECDSA
./scripts/aws-deploy.sh deploy
```

Terraform then grants the ECS task role `kms:Sign` + `kms:GetPublicKey` and injects `GUARDIAN_ACK_ECDSA_BACKEND=aws-kms` / `GUARDIAN_ACK_ECDSA_KMS_KEY_ID`. See [`runbooks/secrets.md`](./runbooks/secrets.md#hosted-ecdsa-backend-aws-kms) for key lifecycle, the immutable-spec caveat, and migrating an existing deployment (a new keypair, so a `SwitchGuardian` identity change).

Dashboard operator public keys use a separate optional secret. The easiest
deployment path is to pass the public keys to Terraform and let it create the
stack-scoped secret:

```bash
export GUARDIAN_OPERATOR_PUBLIC_KEYS_JSON='["0x<alice-falcon-public-key>","0x<bob-falcon-public-key>"]'
```

or in `terraform.tfvars`:

```hcl
guardian_operator_public_keys = [
  "0x<alice-falcon-public-key>",
  "0x<bob-falcon-public-key>"
]
```

If you already manage the secret outside this stack, pass its ARN through
`GUARDIAN_OPERATOR_PUBLIC_KEYS_SECRET_ARN` or
`guardian_operator_public_keys_secret_arn`. An explicit secret ARN takes
precedence over the Terraform-managed public key list.

The ECS task role is granted read access only to the configured secret ARN. The
server rereads that secret during operator auth checks, so adding or removing a
key in the existing secret takes effect without an application restart. When
Terraform manages the secret, update the public key list and rerun deploy.

EVM deployments need the `evm` server feature plus server-owned chain config.
By default, `scripts/aws-deploy.sh` derives allowed chain IDs, RPC URLs, and the
shared EntryPoint address from `config/evm/chains.json`. It passes RPC URLs to
Terraform as a stack-scoped Secrets Manager secret and the EntryPoint address as
a normal ECS environment variable. To use an alternate JSON file, set
`GUARDIAN_EVM_CHAIN_CONFIG_FILE`.

You can still override the derived values by setting
`GUARDIAN_EVM_ALLOWED_CHAIN_IDS`, `GUARDIAN_EVM_RPC_URLS`, or
`GUARDIAN_EVM_ENTRYPOINT_ADDRESS` directly, or by passing existing secret ARNs
through `GUARDIAN_EVM_ALLOWED_CHAIN_IDS_SECRET_ARN` and
`GUARDIAN_EVM_RPC_URLS_SECRET_ARN`.

When an EVM UI runs on a different origin, set
`GUARDIAN_CORS_ALLOWED_ORIGINS` to a comma-separated list of exact origins.
Wildcard origins are rejected. When this value is configured, the server enables
credentialed CORS so browsers can include the host-only, `HttpOnly`
`guardian_evm_session` cookie.

If you still have an older local state file at `infra/terraform.tfstate`, move it manually before using the split-state workflow:

```bash
cp infra/terraform.tfstate infra/terraform.guardian.dev.tfstate
cp infra/terraform.tfstate.backup infra/terraform.guardian.dev.tfstate.backup 2>/dev/null || true
```

Use `--skip-build` when the image already exists in ECR and you only need infra/runtime changes, or when you are applying immediately after a reviewed `plan`:

```bash
./scripts/aws-deploy.sh deploy --skip-build
```

For benchmark-oriented production deploys, prefer explicit overrides rather than changing the base prod profile in code. A typical starting point is:

```bash
set -a && source .env && set +a

export DEPLOY_STAGE=prod
export STACK_NAME=guardian-prod
export TF_VAR_server_cpu=2048
export TF_VAR_server_memory=4096
export TF_VAR_server_desired_count=3
export TF_VAR_server_autoscaling_min_capacity=3
export TF_VAR_server_autoscaling_max_capacity=10
export TF_VAR_rds_instance_class=db.r6g.large
export TF_VAR_rds_allocated_storage=100
export TF_VAR_rds_max_allocated_storage=400
export TF_VAR_rds_proxy_subnet_ids='["subnet-25c1722b","subnet-4d0eca6c"]'
export TF_VAR_guardian_db_pool_max_size=64
export TF_VAR_guardian_metadata_db_pool_max_size=64
export TF_VAR_guardian_rate_limit_enabled=false

./scripts/aws-deploy.sh deploy --skip-build
```

## Validate

```bash
./scripts/aws-deploy.sh status
curl https://guardian.openzeppelin.com/pubkey
grpcurl -import-path crates/server/proto -proto guardian.proto -d '{}' guardian.openzeppelin.com:443 guardian.Guardian/GetPubkey
```

## Metrics, Dashboard, And Alarms

Application metrics ship to CloudWatch by default. Two switches control
this: `guardian_metrics_enabled` turns on the server's Prometheus endpoint,
and `cloudwatch_metrics_enabled` deploys the ADOT sidecar, EMF log group,
IAM policy, dashboard, and alarms on top of it. The export pipeline
cascades off with the endpoint, so `guardian_metrics_enabled = false` alone
turns everything off. Disabling only `cloudwatch_metrics_enabled` keeps the
endpoint without publishing CloudWatch custom metrics — but note the
endpoint stays **loopback-only**, so that mode is useful only for an
alternative in-task collector you add by customizing the module; the stack
exposes no knobs for a routable bind address.

- The server runs with `GUARDIAN_METRICS_ENABLED=true`, serving the Prometheus
  exposition on `127.0.0.1:9464/metrics`. Fargate `awsvpc` containers share one
  network namespace, so the endpoint is **loopback-only**: the sidecar reaches
  it on `127.0.0.1` while nothing outside the task can — it is never exposed via
  the ALB, target groups, or security groups. An externally scraped setup would
  additionally require an explicit bind address, restricted security-group
  ingress, and `GUARDIAN_METRICS_BEARER_TOKEN`; this module deliberately
  configures none of that.
- Latency and other duration metrics are **windowed**: the collector
  delta-converts the Prometheus histograms (the awsemf exporter does not do
  this for histograms on its own), so CloudWatch `Average` over a 5-minute
  period reflects that period, not the process lifetime.
- An **AWS Distro for OpenTelemetry (ADOT) Collector sidecar** in the server
  task scrapes the endpoint every 60s and exports a curated selection of
  metrics to CloudWatch as EMF log events (log group `/ecs/<service>/emf`).
  CloudWatch materializes them as custom metrics under the
  `metrics_namespace` namespace, which is **per stack**: `Guardian/Server` for
  the default stack name, `Guardian-Prod/Server` for `guardian-prod`, and so
  on, so stacks in one account never mix metrics.
  The collector config is injected via `AOT_CONFIG_CONTENT`
  (`infra/observability.tf`); no custom image or SSM parameter is involved.
  Dimension sets come from Guardian's closed label sets (status, code, outcome,
  kind, event, pool, transport); high-cardinality labels (route, method,
  operation) are rolled up to keep the custom-metric count and cost bounded.
  No scrape bearer token is configured on this path: network isolation (the
  loopback-only listener inside the task) is the first defense layer described
  in [the observability guide](./guides/observability/README.md#protecting-the-endpoint-production),
  and no other network path to the endpoint exists.
- The sidecar is **non-essential** (its exit does not stop the task) and its
  memory is capped at 256 MiB, which bounds its worst-case share of the shared
  task envelope — it is a bound, not isolation: both containers still draw
  from the task's `server_memory` total, so size that with ~256 MiB of
  headroom in mind. If the collector leaks it is OOM-killed at its cap, the
  server keeps serving, and the `metrics-missing` alarm fires on the fleet
  going dark. With more than one task, a single dead sidecar is not detected
  by that alarm — the remaining tasks keep the metrics alive and the fleet's
  Sums/Averages skew until the next deployment; per-task detection is a
  possible follow-up.
- Terraform creates a CloudWatch **dashboard** named `<stack>-server` (request
  volume, error rate, latency, proposal/delta lifecycle, canonicalization
  health, storage and DB-pool health, Miden RPC, ECS CPU/memory/tasks) and
  these **alarms**:

| Alarm | Fires when |
|-------|------------|
| `<stack>-http-5xx-rate` | HTTP 5xx responses (500/501/502/503/504) exceed `alarm_error_rate_threshold_percent` (default 5%) of requests for 15 min. ALB health checks count as successful requests and dilute the rate on low-traffic multi-task fleets — treat as a sustained-fault signal |
| `<stack>-grpc-error-rate` | gRPC server-fault responses (`internal`, `unavailable`, `unknown`, `data_loss`, `deadline_exceeded`) exceed the same threshold for 15 min; the same health-check dilution applies |
| `<stack>-http-latency` | Average HTTP latency exceeds `alarm_latency_threshold_seconds` (default 1s) for 15 min. Fleet average across all routes — continuous ALB health-check probes dilute it on low-traffic stacks, so treat it as a sustained-degradation signal |
| `<stack>-canonicalization-failures` | Canonicalization passes (full, fast, or reconcile) report `error` or `partial` (some accounts failed) outcomes for 10 min |
| `<stack>-metrics-missing` | Application metrics stop arriving — the constant `guardian_build_info` heartbeat disappears (metrics endpoint down, sidecar dead, or scrape failing) |
| `<stack>-metrics-refresh-failures` | Slow-aggregate refresher attempts are failing; delta/proposal/account gauges are stale |
| `<stack>-metrics-refresh-stale` | The refresh timestamp stopped advancing for ≥ 10 min (hung or dead refresher — catches what the failures counter cannot) |
| `<stack>-ecs-cpu-high` / `<stack>-ecs-memory-high` | ECS service average CPU/memory exceeds `alarm_cpu_threshold_percent` (85%) / `alarm_memory_threshold_percent` (90%); must sit above the autoscaling targets (enforced at plan time) |

To receive notifications, point the alarms at one or more SNS topics:

```hcl
alarm_actions = ["arn:aws:sns:us-east-1:123456789012:guardian-alerts"]
```

This stack does **not** provision the SNS topic or a chat integration. The
supported notification path is: CloudWatch alarm → an existing **same-region
SNS topic** (listed in `alarm_actions`) → [Amazon Q Developer in chat
applications](https://docs.aws.amazon.com/chatbot/latest/adminguide/what-is.html)
(formerly AWS Chatbot) subscribed to that topic → Slack channel. Create the
topic and the chat subscription out of band, then pass the topic ARN here;
alarms fire both `alarm_actions` and `ok_actions`, so the channel sees
recovery too.

Set `guardian_metrics_enabled = false` to turn everything off (no metrics env
vars, no sidecar, no dashboard, no alarms — the CloudWatch flag cascades off
with it), or only `cloudwatch_metrics_enabled = false` to keep the
loopback-only endpoint without any CloudWatch export (see the caveat above
about what that mode is useful for).

### Verify metrics after a deploy

```bash
# Every name below is per stack; read them all from Terraform outputs.
NS=$(terraform -chdir=infra output -raw metrics_namespace)
DASH=$(terraform -chdir=infra output -raw metrics_dashboard_name)
LOG_GROUP=$(terraform -chdir=infra output -raw server_log_group)
ALARM=$(terraform -chdir=infra output -raw metrics_missing_alarm_name)

# 1. Metrics arriving in the namespace (allow ~2 minutes after task start)
aws cloudwatch list-metrics --namespace "$NS" --output table | head -40

# 2. Dashboard exists and populates
aws cloudwatch get-dashboard --dashboard-name "$DASH" --query DashboardName
# then open CloudWatch > Dashboards > <stack>-server in the console

# 3. Sidecar logs — scrape failures surface here as
#    "Failed to scrape Prometheus endpoint"
aws logs tail "$LOG_GROUP" --log-stream-name-prefix adot --since 15m

# 4. Exercise one alarm notification path end to end
aws cloudwatch set-alarm-state --alarm-name "$ALARM" \
  --state-value ALARM --state-reason "notification path test"
# the next evaluation returns it to OK automatically
```

## Operations

### Logs

```bash
./scripts/aws-deploy.sh logs
```

### Status

```bash
./scripts/aws-deploy.sh status
```

The script reads the state file for the active `STACK_NAME` and `DEPLOY_STAGE`. The current default path is:

```text
infra/terraform.<stack>.<stage>.tfstate
```

You can override that with `TF_STATE_PATH` if needed.

### Destroy

```bash
./scripts/aws-deploy.sh cleanup
```

In the prod stage the RDS instance has deletion protection on and takes a
final snapshot (`<stack>-postgres-final`) on destroy, so a prod cleanup
fails until you set `TF_VAR_rds_deletion_protection=false` and re-apply.
Restoring from the final snapshot is covered in
[`runbooks/backup-restore.md`](./runbooks/backup-restore.md).

ECR repositories are not managed by Terraform:

```bash
aws ecr delete-repository --repository-name guardian-server --force --region us-east-1
```

## Resources Created

| Resource | Description |
|----------|-------------|
| ECS Cluster | Fargate cluster derived from `stack_name` |
| ECS Service | Guardian server service |
| Application Load Balancer | Internet-facing ALB derived from `stack_name` |
| Target Groups | HTTP target group for port `3000` and gRPC target group for port `50051` |
| RDS | Managed PostgreSQL instance and subnet group |
| RDS Proxy | Managed PostgreSQL proxy in the production profile |
| Secrets Manager | Secret containing `DATABASE_URL` for the server task |
| Secrets Manager | Optional operator public keys secret for dashboard auth |
| Secrets Manager | Optional EVM allowed chain IDs and RPC URLs secrets |
| Secrets Manager | Secrets containing the Falcon and ECDSA ack private keys used to seed the server keystore in prod |
| Security Groups | ALB, server, and database security groups |
| CloudWatch Log Groups | Cluster execute-command logs, server logs, and the EMF metrics log group |
| IAM Role | ECS task execution and runtime roles |
| ADOT Sidecar | OpenTelemetry Collector container in the server task exporting Prometheus metrics to CloudWatch |
| CloudWatch Dashboard | `<stack>-server` application and ECS overview |
| CloudWatch Alarms | Error rate, latency, canonicalization, metrics pipeline, and ECS saturation alarms |

## Outputs

| Output | Description |
|--------|-------------|
| `alb_dns_name` | ALB DNS name |
| `alb_url` | Full ALB URL |
| `custom_domain_url` | Canonical service URL: https with a certificate, http when Terraform manages only the DNS record |
| `grpc_endpoint` | Public gRPC endpoint when HTTPS is enabled |
| `database_endpoint` | RDS endpoint used by the server |
| `rds_proxy_endpoint` | RDS Proxy endpoint when enabled |
| `rds_instance_class` | Effective RDS instance class |
| `rds_allocated_storage` | Effective allocated RDS storage in GiB |
| `database_url_secret_arn` | Secrets Manager ARN for the server `DATABASE_URL` |
| `operator_public_keys_secret_arn` | Secrets Manager ARN used for dashboard operator public keys |
| `operator_public_keys_secret_name` | Terraform-managed operator public keys secret name, when created |
| `guardian_evm_allowed_chain_ids_secret_arn` | Secrets Manager ARN used for EVM allowed chain IDs |
| `guardian_evm_rpc_urls_secret_arn` | Secrets Manager ARN used for EVM RPC URLs |
| `guardian_evm_entrypoint_address` | Shared EVM EntryPoint address configured for the server |
| `guardian_cors_allowed_origins` | Explicit CORS origins configured for the server |
| `ack_falcon_secret_name` | Secrets Manager name for the Falcon ack key |
| `ack_ecdsa_secret_name` | Secrets Manager name for the ECDSA ack key |
| `dashboard_cursor_secret_name` | Secrets Manager name for the shared dashboard cursor key |
| `ecs_cluster_arn` | ECS cluster ARN |
| `server_service_arn` | Server ECS service ARN |
| `metrics_namespace` | CloudWatch namespace receiving Guardian application metrics |
| `metrics_dashboard_name` | CloudWatch dashboard name |
| `metrics_emf_log_group` | Log group the ADOT sidecar writes EMF metric events into |
| `metrics_missing_alarm_name` | Name of the metrics-pipeline heartbeat alarm for this stack |

## Stage Profiles

### Dev

- single ECS task
- no ECS autoscaling
- direct ECS to RDS connection
- no RDS Proxy
- conservative Guardian runtime limits

### Prod

- higher ECS desired count
- ECS service autoscaling
- larger default RDS instance class and base storage
- RDS storage autoscaling
- RDS Proxy between ECS and RDS
- higher Guardian runtime rate-limit and DB-pool defaults for benchmark traffic

#### Horizontal scaling (multiple replicas)

The prod profile runs 2–6 tasks behind the ALB. Because it sets `GUARDIAN_ENV=prod`
and the Postgres backend, the server runs **shared coordination** (sessions,
login challenges, and the canonicalization lease live in Postgres) — so any
request lands on any replica and canonicalization runs on exactly one replica at
a time. Terraform also sets `GUARDIAN_MAX_REPLICAS` from
`effective_guardian_max_replicas` (the greater of desired count and autoscaling
max, 6 by default) so global HTTP and dashboard commitment rate limits are
partitioned across the steady-state fleet. A rolling deployment may allow up to
`server_deployment_maximum_percent / 100` times the configured aggregate limit
(2× by default). The default dashboard share is 5 requests per minute on a
keep-alive-pinned replica. In prod, Terraform requires the pre-created dashboard
cursor secret and injects the same value into every task, so dashboard
pagination works across replicas. The server itself still warns and uses an
ephemeral key when run without the variable outside this managed prod profile.
Watch the per-replica `GUARDIAN_DB_POOL_MAX_SIZE` against Postgres
`max_connections` (RDS Proxy absorbs most of this). Full operator guidance:
[`runbooks/horizontal-scaling.md`](./runbooks/horizontal-scaling.md).

## HTTPS And gRPC

HTTPS is enabled when `acm_certificate_arn` is set. DNS can be managed through Cloudflare, Route 53, or both depending on which variables are provided.

When HTTPS is enabled, the ALB routes standard HTTPS requests to the server HTTP port `3000` and gRPC requests for `/guardian.Guardian/*` to the server gRPC port `50051`. The public gRPC endpoint uses the same hostname on port `443`.

On Apple Silicon hosts, `CPU_ARCHITECTURE=X86_64` builds are slower because Docker builds `linux/amd64` images under emulation. Switching to `ARM64` avoids that local emulation cost, but it also changes the ECS task runtime architecture.

## Migrating An Existing ECS-Postgres Stack

The current Terraform configuration is RDS-only. There is no supported dual-mode deployment that keeps the old ECS Postgres service alive after apply.

Use this cutover flow for an existing stack:

1. Capture the current stack state:
   ```bash
   ./scripts/aws-deploy.sh status
   ```
2. Create a logical PostgreSQL backup from the existing ECS-hosted database before applying the updated stack.
3. Apply the updated RDS-backed Terraform stack:
   ```bash
   ./scripts/aws-deploy.sh deploy --skip-build
   ```
4. Restore the backup into the new RDS database.
5. Validate the public service:
   ```bash
   ./scripts/aws-deploy.sh status
   curl https://<host>/pubkey
   grpcurl -import-path crates/server/proto -proto guardian.proto -d '{}' <host>:443 guardian.Guardian/GetPubkey
   ```
6. Confirm the old Postgres ECS service and Cloud Map database-discovery resources are gone from AWS before treating the cutover as complete.

## Troubleshooting

- If the server task fails during startup, check `./scripts/aws-deploy.sh logs` first and confirm the reported `database_endpoint` matches the expected RDS host.
- If a prod deploy fails before Terraform starts, confirm the fixed prod ACK secrets exist by running `./scripts/aws-deploy.sh bootstrap-ack-keys` once and then retrying the deploy.
- If RDS subnet-group creation fails, verify the selected subnets cover at least two subnets for the database deployment.
- If gRPC works against the ALB directly but fails on the public hostname, check Cloudflare gRPC settings on the zone.
- If the `metrics-missing` alarm fires or the dashboard is empty, tail the sidecar stream (`aws logs tail /ecs/<service> --log-stream-name-prefix adot`) — scrape failures appear as `Failed to scrape Prometheus endpoint`, and export failures reference `awsemf`.

## Legacy Script

The legacy deployment logic has been replaced by the Terraform-backed `scripts/aws-deploy.sh`.
