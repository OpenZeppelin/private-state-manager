# GUARDIAN Server AWS Infrastructure (Terraform)

This directory contains the Terraform configuration for the current Guardian AWS deployment: ECS/Fargate behind an ALB, backed by Amazon RDS for PostgreSQL.

The deployment is stage-aware:
- `deployment_stage = "dev"` keeps the stack close to the current fixed-capacity profile
- `deployment_stage = "prod"` enables ECS autoscaling, RDS storage autoscaling, RDS Proxy, and larger default RDS sizing for benchmark traffic

## Architecture

```text
Internet → ALB (HTTP/HTTPS + gRPC over HTTPS) → ECS Service (server) → RDS PostgreSQL
```

Resources created:
- ECS Cluster (Fargate)
- ECS service derived from `stack_name` for the Guardian server
- Application Load Balancer with HTTP and gRPC target groups
- RDS PostgreSQL instance and subnet group
- Secrets Manager secret for `DATABASE_URL`
- Optional Secrets Manager secret for dashboard operator public keys
- Optional Secrets Manager secrets for EVM allowed chain IDs and RPC URLs
- Secrets Manager secrets for stable Falcon and ECDSA ack keys in `prod`
- Pre-created Secrets Manager secret for the shared dashboard cursor key in `prod`
- Security groups for the ALB, server task, and database
- CloudWatch log groups
- IAM roles for ECS task execution and runtime
- ADOT Collector sidecar in the server task exporting Guardian Prometheus metrics to CloudWatch (EMF)
- CloudWatch dashboard (`<stack>-server`) and alarms (error rate, latency, canonicalization, metrics pipeline, ECS saturation)

The Guardian metrics endpoint binds loopback inside the task's shared network
namespace; only the sidecar can reach it — it is never exposed via the ALB or
security groups. See `observability.tf` and
[`docs/SERVER_AWS_DEPLOY.md`](../docs/SERVER_AWS_DEPLOY.md#metrics-dashboard-and-alarms)
for details and verification steps.

## Usage

### 1. Build And Push Docker Image

```bash
export AWS_REGION=us-east-1
export CPU_ARCHITECTURE=X86_64
export AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)

export STACK_NAME=guardian
export ECR_REPO_NAME="${STACK_NAME}-server"

aws ecr create-repository --repository-name "$ECR_REPO_NAME" --region "$AWS_REGION" 2>/dev/null || true

aws ecr get-login-password --region "$AWS_REGION" | \
  docker login --username AWS --password-stdin "$AWS_ACCOUNT_ID.dkr.ecr.$AWS_REGION.amazonaws.com"

docker build --platform linux/amd64 -t "$ECR_REPO_NAME" .
docker tag "$ECR_REPO_NAME:latest" "$AWS_ACCOUNT_ID.dkr.ecr.$AWS_REGION.amazonaws.com/$ECR_REPO_NAME:latest"
docker push "$AWS_ACCOUNT_ID.dkr.ecr.$AWS_REGION.amazonaws.com/$ECR_REPO_NAME:latest"
```

### 2. Configure Variables

Create `terraform.tfvars`:

```hcl
aws_region = "us-east-1"

# Optional: image/task architecture
# cpu_architecture = "X86_64"
# cpu_architecture = "ARM64"

# Optional: stack base name
# stack_name = "guardian"

# Optional: deployment profile
# deployment_stage = "dev"
# deployment_stage = "prod"

server_image_uri = "123456789012.dkr.ecr.us-east-1.amazonaws.com/guardian-server@sha256:..."

# Optional: Use specific VPC/subnets
# vpc_id     = "vpc-xxxxxxxx"
# subnet_ids = ["subnet-xxxxxxxx", "subnet-yyyyyyyy"]
# rds_proxy_subnet_ids = ["subnet-xxxxxxxx", "subnet-yyyyyyyy"]
# In us-east-1, avoid subnets in us-east-1e/use1-az3 for RDS Proxy.

# Optional: Postgres credentials
# postgres_db       = "guardian"
# postgres_user     = "guardian"
# postgres_password = "guardian_dev_password"

# Optional: RDS sizing overrides
# Stage defaults:
# - dev  -> db.t3.micro, 20 GiB allocated, no storage autoscaling ceiling
# - prod -> db.t3.medium, 50 GiB allocated, 200 GiB max allocated
# rds_instance_class = "db.t3.medium"
# rds_allocated_storage = 50
# rds_max_allocated_storage = 200
# rds_proxy_enabled = true

# Optional: stage/runtime capacity overrides
# server_desired_count = 2
# server_autoscaling_enabled = true
# server_autoscaling_min_capacity = 2
# server_autoscaling_max_capacity = 6
# guardian_rate_limit_enabled = false
# guardian_rate_burst_per_sec = 200
# guardian_rate_per_min = 5000
# guardian_db_pool_max_size = 32
# guardian_metadata_db_pool_max_size = 32
# guardian_canonicalization_fast_promotion_enabled = false

# Optional: dashboard operator Falcon public keys managed by Terraform
# guardian_operator_public_keys = [
#   "0x<alice-falcon-public-key>",
#   "0x<bob-falcon-public-key>",
# ]

# Optional: existing dashboard operator Falcon public keys secret
# guardian_operator_public_keys_secret_arn = "arn:aws:secretsmanager:us-east-1:123456789012:secret:guardian/operators"

# Optional: EVM runtime configuration when using Terraform directly.
# scripts/aws-deploy.sh derives these from config/evm/chains.json by default.
# guardian_evm_allowed_chain_ids = "1,11155111"
# guardian_evm_rpc_urls = "1=https://ethereum-rpc.publicnode.com,11155111=https://ethereum-sepolia-rpc.publicnode.com"
# guardian_evm_entrypoint_address = "0x433709009b8330fda32311df1c2afa402ed8d009"
# guardian_cors_allowed_origins = "https://accounts.openzeppelin.com"

# Optional: Route 53 hosted zone ID
# route53_zone_id = "Z1234567890ABC"
```

### 3. Deploy

```bash
cd infra
terraform init
terraform plan
terraform apply
```

Image-only rollouts of a published GHCR version go through the **AWS Deploy**
GitHub Actions workflow instead (OIDC, no local credentials or state). It also
moves the ECR `latest` tag so the `server_image_uri` resolved by
`scripts/aws-deploy.sh` stays in step with what is running. See
[`docs/SERVER_AWS_DEPLOY.md`](../docs/SERVER_AWS_DEPLOY.md#deploying-a-published-image-from-github-actions).

For direct prod Terraform usage, create the dashboard cursor secret before
planning. Its value must be exactly 64 hexadecimal characters. Terraform checks
that the secret exists without reading its value into state; the deployment
helper performs the value-shape validation. If a customer-managed KMS key
encrypts the secret, its key policy must allow the ECS task execution role to
decrypt it.

When you use `scripts/aws-deploy.sh`, it keeps separate local Terraform state files per `STACK_NAME` and `deployment_stage`, using:

```text
infra/terraform.<stack>.<stage>.tfstate
```

by default.

If you still have an older local state file at `infra/terraform.tfstate`, move it manually before switching to the split-state workflow:

```bash
cp infra/terraform.tfstate infra/terraform.guardian.dev.tfstate
cp infra/terraform.tfstate.backup infra/terraform.guardian.dev.tfstate.backup 2>/dev/null || true
```

For `prod`, create the ACK key and dashboard cursor secrets once before the first
deploy:

```bash
DEPLOY_STAGE=prod ./scripts/aws-deploy.sh bootstrap-ack-keys
DEPLOY_STAGE=prod ./scripts/aws-deploy.sh bootstrap-dashboard-cursor-secret
```

Normal deploys do not create or rotate these secrets. Terraform requires the
cursor secret to exist, grants the ECS execution role access, and injects its
value as `GUARDIAN_DASHBOARD_CURSOR_SECRET` into every prod task.

By default the secret names follow `${STACK_NAME}/server/ack-{falcon,ecdsa}-secret-key`, so each stack gets its own pair (`guardian-prod` → `guardian-prod/server/ack-falcon-secret-key`, etc.) and multiple Guardian instances can coexist in the same AWS account. To pin a stack at an existing legacy name, set `GUARDIAN_ACK_FALCON_SECRET_NAME` and `GUARDIAN_ACK_ECDSA_SECRET_NAME` before `bootstrap-ack-keys` and `deploy` — they flow through to the `guardian_ack_falcon_secret_name` / `guardian_ack_ecdsa_secret_name` Terraform variables.

> The ECS task definition receives the resolved name as the server's `GUARDIAN_ACK_FALCON_SECRET_ID` / `GUARDIAN_ACK_ECDSA_SECRET_ID` env vars — that is the runtime env the server reads to call Secrets Manager. You do not set those directly when deploying via `scripts/aws-deploy.sh`.

### 4. Get Outputs

```bash
terraform output alb_dns_name
terraform output
```

### 5. Test

```bash
ALB_DNS=$(terraform output -raw alb_dns_name)
CUSTOM_DOMAIN_URL=$(terraform output -raw custom_domain_url)
GRPC_ENDPOINT=$(terraform output -raw grpc_endpoint)
ALIAS_DOMAIN_URL=$(terraform output -raw alias_domain_url 2>/dev/null || true)

# ACM certificates never cover the raw ALB hostname, so the direct checks
# stay on plain HTTP; on HTTPS stacks they return the 301 redirect and the
# hostname checks below verify TLS end to end.
curl --fail-with-body "http://$ALB_DNS/"
curl --fail-with-body "http://$ALB_DNS/pubkey"
[ -z "$CUSTOM_DOMAIN_URL" ] || curl --fail-with-body "$CUSTOM_DOMAIN_URL/pubkey"
[ -z "$GRPC_ENDPOINT" ] || grpcurl -import-path ../crates/server/proto -proto guardian.proto -d '{}' "${GRPC_ENDPOINT#https://}:443" guardian.Guardian/GetPubkey
[ -z "$ALIAS_DOMAIN_URL" ] || curl --fail-with-body "$ALIAS_DOMAIN_URL/pubkey"
[ -z "$ALIAS_DOMAIN_URL" ] || grpcurl -import-path ../crates/server/proto -proto guardian.proto -d '{}' "${ALIAS_DOMAIN_URL#https://}:443" guardian.Guardian/GetPubkey
```

### 6. Destroy

```bash
terraform destroy
```

In the prod stage the RDS instance has deletion protection on and takes a
final snapshot (`<stack>-postgres-final`) on destroy. To tear down a prod
stack, set `TF_VAR_rds_deletion_protection=false`, apply, then destroy. If a
snapshot named `<stack>-postgres-final` is left over from a previous
teardown, copy or delete it first — the destroy fails on the name collision.

ECR repositories are not managed by Terraform:

```bash
aws ecr delete-repository --repository-name "$ECR_REPO_NAME" --force --region "$AWS_REGION"
```

## Variables Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `aws_region` | `us-east-1` | AWS region |
| `cpu_architecture` | `X86_64` | ECS task and image architecture |
| `stack_name` | `guardian` | Base name used to derive stack resource names |
| `deployment_stage` | `dev` | Deployment stage profile |
| `server_image_uri` | (required) | ECR image URI for the server, preferably pinned to a digest |
| `guardian_operator_public_keys` | `[]` | Falcon public keys used to create a stack-scoped operator public keys secret |
| `guardian_operator_public_keys_secret_arn` | `""` | Existing operator public keys secret ARN; takes precedence over the managed list |
| `guardian_evm_allowed_chain_ids` | `""` | EVM chain IDs used to create a stack-scoped allowed chain IDs secret |
| `guardian_evm_allowed_chain_ids_secret_arn` | `""` | Existing EVM allowed chain IDs secret ARN; takes precedence over the managed value |
| `guardian_evm_rpc_urls` | `""` | EVM `chain_id=url` entries used to create a stack-scoped RPC URLs secret |
| `guardian_evm_rpc_urls_secret_arn` | `""` | Existing EVM RPC URLs secret ARN; takes precedence over the managed value |
| `guardian_evm_entrypoint_address` | `""` | Shared EVM EntryPoint address injected into the server task |
| `guardian_cors_allowed_origins` | `""` | Comma-separated explicit HTTP origins allowed by credentialed CORS |
| `vpc_id` | (default VPC) | VPC ID |
| `subnet_ids` | (all subnets in VPC) | Subnet IDs for ECS tasks and ALB |
| `rds_proxy_subnet_ids` | filtered `subnet_ids` | Optional dedicated subnet IDs for RDS Proxy |
| `postgres_db` | `guardian` | Postgres database name |
| `postgres_user` | `guardian` | Postgres username |
| `postgres_password` | `guardian_dev_password` | Postgres password |
| `rds_instance_class` | `db.t3.micro` in dev, `db.r6g.large` in prod | RDS instance class |
| `rds_allocated_storage` | `20` in dev, `50` in prod | RDS allocated storage in GiB |
| `rds_max_allocated_storage` | `null` in dev, `200` in prod | RDS storage autoscaling ceiling |
| `rds_backup_retention_days` | `7` | Automated backup retention in days; enables point-in-time recovery |
| `rds_deletion_protection` | `false` in dev, `true` in prod | Whether the RDS instance is protected from deletion |
| `rds_skip_final_snapshot` | `true` in dev, `false` in prod | Whether destroying the RDS instance skips the final snapshot |
| `rds_multi_az` | `false` | Whether RDS runs as a Multi-AZ deployment with a standby replica |
| `rds_proxy_enabled` | `false` in dev, `true` in prod | Whether RDS Proxy is enabled |
| `domain_name` | `openzeppelin.com` | Root domain for HTTPS endpoint |
| `subdomain` | `guardian` | Subdomain for HTTPS endpoint |
| `acm_certificate_arn` | `""` | ACM certificate for the canonical hostname |
| `route53_zone_id` | `""` | Route 53 hosted zone ID for the domain |
| `cloudflare_zone_id` | `""` | Cloudflare zone ID for the canonical hostname |
| `alias_subdomain` | `""` | Migration-only legacy subdomain under `domain_name`; leave empty normally |
| `alias_acm_certificate_arn` | `""` | Migration-only distinct legacy certificate attached through SNI |
| `alb_ingress_cidrs` | `["0.0.0.0/0"]` | CIDR blocks allowed to reach the ALB |
| `server_cpu` | `512` | Server task CPU units |
| `server_memory` | `1024` | Server task memory (MB) |
| `server_desired_count` | `1` in dev, `2` in prod | ECS service desired task count |
| `server_autoscaling_enabled` | `false` in dev, `true` in prod | Whether ECS autoscaling is enabled |
| `guardian_rate_limit_enabled` | `true` | Whether Guardian rate limiting is enabled (HTTP and gRPC) |
| `guardian_rate_burst_per_sec` | `10` in dev, `200` in prod | Guardian HTTP burst limit |
| `guardian_rate_per_min` | `60` in dev, `5000` in prod | Guardian HTTP sustained limit |
| `guardian_max_replicas` | greater of desired count and autoscaling max when enabled, desired count otherwise (prod `6` by default) | `GUARDIAN_MAX_REPLICAS` rate-limit partition divisor; an explicit value is clamped **up** to steady-state capacity. A rolling deployment may temporarily allow up to `server_deployment_maximum_percent / 100` times the configured fleet limit. |
| `guardian_dashboard_commitment_rate_burst_per_sec` | `6` | Fleet-wide dashboard per-commitment burst budget, partitioned by the steady-state replica capacity |
| `guardian_dashboard_commitment_rate_per_min` | `30` | Fleet-wide dashboard per-commitment sustained budget, partitioned by the steady-state replica capacity |
| `server_deployment_maximum_percent` | `200` | ECS rolling-deploy task ceiling (percent of desired count). The temporary fleet-wide rate allowance can scale by the same factor during a rollout. Must be an integer in `(100, 200]` and allow at least one surge task at the minimum positive desired capacity, including autoscaling minimum |
| `guardian_db_pool_max_size` | `16` in dev, `32` in prod | Guardian storage DB pool size |
| `guardian_metadata_db_pool_max_size` | matches storage by default | Guardian metadata DB pool size |
| `guardian_canonicalization_fast_promotion_enabled` | `true` | Enables the recent-candidate promotion-only pass in the ECS task definition |
| `guardian_log_format` | `json` | Log format for `GUARDIAN_LOG_FORMAT` (`text`, `json`, `compact`) |
| `log_retention_days` | `7` | CloudWatch log retention in days |
| `guardian_metrics_enabled` | `true` | Guardian Prometheus metrics endpoint (loopback-only inside the task) |
| `cloudwatch_metrics_enabled` | `true` | ADOT sidecar + EMF export + CloudWatch dashboard/alarms (cascades off when the endpoint is disabled) |
| `adot_image` | pinned ADOT Collector release | Digest-pinned sidecar image |
| `metrics_namespace` | `<Title(stack_name)>/Server` | CloudWatch namespace for application metrics |
| `alarm_actions` | `[]` | ARNs (e.g. SNS topics) notified on alarm/ok transitions |
| `alarm_error_rate_threshold_percent` | `5` | HTTP 5xx / gRPC error-rate alarm threshold |
| `alarm_latency_threshold_seconds` | `1` | Average HTTP latency alarm threshold |
| `alarm_cpu_threshold_percent` | `85` | ECS CPU saturation alarm threshold |
| `alarm_memory_threshold_percent` | `90` | ECS memory saturation alarm threshold |

## Outputs

| Output | Description |
|--------|-------------|
| `alb_dns_name` | ALB DNS name for accessing the server |
| `alb_url` | Full URL (http or https) |
| `custom_domain_url` | Canonical service URL: https with a certificate, http when Terraform manages only the DNS record |
| `alias_domain_url` | Migration-only legacy domain URL |
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
| `server_log_group` | CloudWatch log group for the server |
| `cluster_log_group` | CloudWatch log group for ECS execute command |
| `guardian_rate_limit_enabled` | Whether rate limiting is enabled (HTTP and gRPC) |
| `guardian_max_replicas` | Effective `GUARDIAN_MAX_REPLICAS` after clamping to steady-state capacity |
| `guardian_dashboard_commitment_rate_burst_per_sec` | Effective fleet-wide dashboard per-commitment burst budget |
| `guardian_dashboard_commitment_rate_per_min` | Effective fleet-wide dashboard per-commitment sustained budget |
| `guardian_metrics_enabled` | Whether the Guardian Prometheus metrics endpoint is enabled |
| `cloudwatch_metrics_enabled` | Whether the metrics sidecar, dashboard, and alarms are deployed |
| `metrics_missing_alarm_name` | Name of the metrics-pipeline heartbeat alarm |
| `metrics_namespace` | CloudWatch namespace receiving Guardian application metrics |
| `metrics_dashboard_name` | CloudWatch dashboard name |
| `metrics_emf_log_group` | Log group the ADOT sidecar writes EMF metric events into |

## Stage Profiles

### Dev

- single ECS task
- direct ECS to RDS connectivity
- no ECS autoscaling
- no RDS Proxy
- conservative runtime rate limits and DB pool sizes

### Prod

- higher ECS desired count
- ECS target-tracking autoscaling
- larger default RDS instance class and base storage
- RDS storage autoscaling
- RDS Proxy between ECS and RDS
- higher runtime rate limits and DB pool sizes for benchmark traffic

## HTTPS Configuration

HTTPS is enabled when `acm_certificate_arn` is set. DNS can be managed through Cloudflare, Route 53, both, or an external provider. Terraform creates canonical and migration-only alias records only for the configured providers; external records remain operator-managed. The migration-only secondary-subdomain settings are covered in [`docs/runbooks/guardian-domain-migration.md`](../docs/runbooks/guardian-domain-migration.md).

When HTTPS is enabled, the ALB routes standard HTTPS requests to the server HTTP port `3000` and gRPC requests for `/guardian.Guardian/*` to the server gRPC port `50051`. The public gRPC endpoint uses the same hostname on port `443`.

On Apple Silicon hosts, building `X86_64` images is slower because Docker builds `linux/amd64` images under emulation. If you want faster native local builds and your ECS deployment can run ARM64, set `cpu_architecture = "ARM64"` and deploy an ARM64 task definition.

## Existing Stack Cutover

This Terraform stack is RDS-only. Existing stacks that still run ECS-hosted Postgres must be migrated with an explicit backup-and-restore cutover:

1. Capture the current outputs with `terraform output` or `./scripts/aws-deploy.sh status`.
2. Create a logical backup from the existing Postgres runtime before applying the updated stack.
3. Apply the updated Terraform stack so the server is wired to RDS.
4. Restore the backup into the RDS database.
5. Validate the public Guardian endpoints.
6. Confirm the old Postgres ECS and Cloud Map resources are gone from AWS before considering the cutover complete.

## Storage encryption key

Optional storage-at-rest encryption (see
[`docs/PRODUCTION.md`](../docs/PRODUCTION.md#storage-encryption)) reads its key
from a Secrets Manager secret named by `GUARDIAN_STORAGE_ENCRYPTION_KEY_SECRET_ID`.

The key material is created out-of-band (it must never enter Terraform state),
then the stack wires the rest automatically, mirroring the ACK key secrets:

1. Bootstrap the key secret once, against an empty store:

   ```bash
   DEPLOY_STAGE=prod STACK_NAME=guardian-prod \
     ./scripts/aws-deploy.sh bootstrap-storage-encryption-key
   ```

2. Enable it on the next deploy by exporting the secret name (the bootstrap
   command prints it):

   ```bash
   GUARDIAN_STORAGE_ENCRYPTION_SECRET_NAME=guardian-prod/server/storage-encryption-key \
     DEPLOY_STAGE=prod STACK_NAME=guardian-prod ./scripts/aws-deploy.sh deploy
   ```

Setting `GUARDIAN_STORAGE_ENCRYPTION_SECRET_NAME` is what enables encryption — the
deploy script passes it through to `guardian_storage_encryption_secret_name`, and
the stack then grants the ECS **task** role `secretsmanager:GetSecretValue` on that
secret and injects `GUARDIAN_STORAGE_ENCRYPTION_KEY_SECRET_ID`. Leaving it unset
keeps storage in plaintext at rest. Wiring is prod-only; dev uses
`GUARDIAN_STORAGE_ENCRYPTION_KEY` directly. Rotation details live in
[`docs/runbooks/secrets.md`](../docs/runbooks/secrets.md#storage-encryption-key).

## Troubleshooting

- If the server task does not start, inspect the server log group and confirm the `database_endpoint` output points to the expected RDS host.
- If RDS subnet-group creation fails, verify the selected subnets cover at least two subnets for the database deployment.
- If gRPC works against the ALB but not the public hostname, check Cloudflare gRPC settings.
