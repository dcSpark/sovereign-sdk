#!/usr/bin/env bash
set -euo pipefail

AWS_PROFILE="${AWS_PROFILE:-}"
AWS_REGION="${AWS_REGION:-}"
FUNCTION_NAME="${FUNCTION_NAME:-}"
ECR_REPOSITORY="${ECR_REPOSITORY:-}"
ROLE_NAME="${ROLE_NAME:-}"
ARCHITECTURE="${ARCHITECTURE:-}"
PLATFORM="${PLATFORM:-}"
MEMORY_SIZE="${MEMORY_SIZE:-}"
TIMEOUT="${TIMEOUT:-}"
EPHEMERAL_STORAGE_MB="${EPHEMERAL_STORAGE_MB:-}"
AUTH_TYPE="${AUTH_TYPE:-}"
FUNCTION_URL_INVOKE_MODE="${FUNCTION_URL_INVOKE_MODE:-}"
NIGHTSTREAM_REF="${NIGHTSTREAM_REF:-}"
NODE_RPC_URL="${NODE_RPC_URL:-}"
BIND_ADDR="${BIND_ADDR:-}"
DA_DB="${DA_DB:-}"
POOL_FVK_PK="${POOL_FVK_PK:-}"
LAMBDA_SUBNET_IDS="${LAMBDA_SUBNET_IDS:-}"
LAMBDA_SECURITY_GROUP_IDS="${LAMBDA_SECURITY_GROUP_IDS:-}"
IMAGE_TAG="${IMAGE_TAG:-}"
SOV_PROOF_VERIFIER_PROOF_BUCKET="${SOV_PROOF_VERIFIER_PROOF_BUCKET:-}"
SOV_PROOF_VERIFIER_PROOF_REGION="${SOV_PROOF_VERIFIER_PROOF_REGION:-}"
SOV_PROOF_VERIFIER_PROOF_ENDPOINT="${SOV_PROOF_VERIFIER_PROOF_ENDPOINT:-}"
SOV_PROOF_VERIFIER_PROOF_PREFIX="${SOV_PROOF_VERIFIER_PROOF_PREFIX:-}"
SOV_PROOF_VERIFIER_PROOF_DOWNLOAD_TTL_SECS="${SOV_PROOF_VERIFIER_PROOF_DOWNLOAD_TTL_SECS:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DOCKERFILE="$REPO_ROOT/crates/utils/sov-proof-verifier-service/Dockerfile"

require_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "Missing required command: $1" >&2
    exit 1
  }
}

require_env() {
  local name="$1"
  if [[ -z "${!name:-}" ]]; then
    echo "Missing required environment variable: $name" >&2
    exit 1
  fi
}

require_cmd aws
require_cmd docker

require_env AWS_PROFILE
require_env AWS_REGION
require_env FUNCTION_NAME
require_env ECR_REPOSITORY
require_env ROLE_NAME
require_env ARCHITECTURE
require_env PLATFORM
require_env MEMORY_SIZE
require_env TIMEOUT
require_env EPHEMERAL_STORAGE_MB
require_env AUTH_TYPE
require_env FUNCTION_URL_INVOKE_MODE
require_env NIGHTSTREAM_REF
require_env NODE_RPC_URL
require_env BIND_ADDR
require_env DA_DB
require_env LAMBDA_SUBNET_IDS
require_env LAMBDA_SECURITY_GROUP_IDS
require_env IMAGE_TAG

ACCOUNT_ID="$(
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws sts get-caller-identity --query Account --output text
)"
ECR_REGISTRY="${ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
IMAGE_URI="${ECR_REGISTRY}/${ECR_REPOSITORY}:${IMAGE_TAG}"
FUNCTION_ENV_KVS="BIND_ADDR=${BIND_ADDR},AWS_LWA_PORT=8080,AWS_LWA_READINESS_CHECK_PATH=/health,AWS_LWA_INVOKE_MODE=response_stream,DA_DB=${DA_DB},NODE_RPC_URL=${NODE_RPC_URL}"
if [[ -n "$POOL_FVK_PK" ]]; then
  FUNCTION_ENV_KVS+=",POOL_FVK_PK=${POOL_FVK_PK}"
fi
if [[ -n "$SOV_PROOF_VERIFIER_PROOF_BUCKET" ]]; then
  FUNCTION_ENV_KVS+=",SOV_PROOF_VERIFIER_PROOF_BUCKET=${SOV_PROOF_VERIFIER_PROOF_BUCKET}"
fi
if [[ -n "$SOV_PROOF_VERIFIER_PROOF_REGION" ]]; then
  FUNCTION_ENV_KVS+=",SOV_PROOF_VERIFIER_PROOF_REGION=${SOV_PROOF_VERIFIER_PROOF_REGION}"
fi
if [[ -n "$SOV_PROOF_VERIFIER_PROOF_ENDPOINT" ]]; then
  FUNCTION_ENV_KVS+=",SOV_PROOF_VERIFIER_PROOF_ENDPOINT=${SOV_PROOF_VERIFIER_PROOF_ENDPOINT}"
fi
if [[ -n "$SOV_PROOF_VERIFIER_PROOF_PREFIX" ]]; then
  FUNCTION_ENV_KVS+=",SOV_PROOF_VERIFIER_PROOF_PREFIX=${SOV_PROOF_VERIFIER_PROOF_PREFIX}"
fi
if [[ -n "$SOV_PROOF_VERIFIER_PROOF_DOWNLOAD_TTL_SECS" ]]; then
  FUNCTION_ENV_KVS+=",SOV_PROOF_VERIFIER_PROOF_DOWNLOAD_TTL_SECS=${SOV_PROOF_VERIFIER_PROOF_DOWNLOAD_TTL_SECS}"
fi
FUNCTION_ENV_VARS="Variables={${FUNCTION_ENV_KVS}}"
LAMBDA_VPC_CONFIG="SubnetIds=${LAMBDA_SUBNET_IDS},SecurityGroupIds=${LAMBDA_SECURITY_GROUP_IDS}"

echo "Using AWS profile: $AWS_PROFILE"
echo "Using AWS region:  $AWS_REGION"
echo "Account ID:        $ACCOUNT_ID"
echo "Function name:     $FUNCTION_NAME"
echo "ECR repository:    $ECR_REPOSITORY"
echo "Image URI:         $IMAGE_URI"
echo "Node RPC URL:      $NODE_RPC_URL"
echo "Function URL mode: $FUNCTION_URL_INVOKE_MODE"
echo "Lambda subnets:    $LAMBDA_SUBNET_IDS"
echo "Lambda SGs:        $LAMBDA_SECURITY_GROUP_IDS"
if [[ -n "$SOV_PROOF_VERIFIER_PROOF_BUCKET" ]]; then
  echo "Proof bucket:      $SOV_PROOF_VERIFIER_PROOF_BUCKET"
fi

if ! AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws ecr describe-repositories --repository-names "$ECR_REPOSITORY" >/dev/null 2>&1; then
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws ecr create-repository \
      --repository-name "$ECR_REPOSITORY" \
      --image-scanning-configuration scanOnPush=true >/dev/null
fi

AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws ecr get-login-password \
  | docker login --username AWS --password-stdin "$ECR_REGISTRY"

docker buildx build \
  --platform "$PLATFORM" \
  --target lambda-runtime \
  --provenance=false \
  --sbom=false \
  --build-arg NIGHTSTREAM_REF="$NIGHTSTREAM_REF" \
  -f "$DOCKERFILE" \
  -t "$IMAGE_URI" \
  --push \
  "$REPO_ROOT"

ROLE_ARN="$(
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws iam get-role --role-name "$ROLE_NAME" --query 'Role.Arn' --output text 2>/dev/null || true
)"

if [[ -z "$ROLE_ARN" || "$ROLE_ARN" == "None" ]]; then
  TRUST_POLICY_FILE="$(mktemp)"
  cat >"$TRUST_POLICY_FILE" <<'JSON'
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Service": "lambda.amazonaws.com"
      },
      "Action": "sts:AssumeRole"
    }
  ]
}
JSON

  ROLE_ARN="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws iam create-role \
        --role-name "$ROLE_NAME" \
        --assume-role-policy-document "file://${TRUST_POLICY_FILE}" \
        --query 'Role.Arn' --output text
  )"
  rm -f "$TRUST_POLICY_FILE"

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws iam attach-role-policy \
      --role-name "$ROLE_NAME" \
      --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole >/dev/null

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws iam attach-role-policy \
      --role-name "$ROLE_NAME" \
      --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole >/dev/null

  # IAM propagation can lag briefly after a new role is created.
  sleep 10
fi

AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws iam attach-role-policy \
    --role-name "$ROLE_NAME" \
    --policy-arn arn:aws:iam::aws:policy/service-role/AWSLambdaVPCAccessExecutionRole >/dev/null 2>&1 || true

if AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws lambda get-function --function-name "$FUNCTION_NAME" >/dev/null 2>&1; then
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda update-function-code \
      --function-name "$FUNCTION_NAME" \
      --image-uri "$IMAGE_URI" >/dev/null

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda update-function-configuration \
      --function-name "$FUNCTION_NAME" \
      --timeout "$TIMEOUT" \
      --memory-size "$MEMORY_SIZE" \
      --ephemeral-storage "{\"Size\": ${EPHEMERAL_STORAGE_MB}}" \
      --environment "$FUNCTION_ENV_VARS" \
      --vpc-config "$LAMBDA_VPC_CONFIG" >/dev/null
else
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda create-function \
      --function-name "$FUNCTION_NAME" \
      --package-type Image \
      --code "ImageUri=${IMAGE_URI}" \
      --role "$ROLE_ARN" \
      --timeout "$TIMEOUT" \
      --memory-size "$MEMORY_SIZE" \
      --architectures "$ARCHITECTURE" \
      --ephemeral-storage "{\"Size\": ${EPHEMERAL_STORAGE_MB}}" \
      --environment "$FUNCTION_ENV_VARS" \
      --vpc-config "$LAMBDA_VPC_CONFIG" >/dev/null
fi

WAIT_CONDITION="function-updated"
if ! AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws lambda wait "$WAIT_CONDITION" --function-name "$FUNCTION_NAME" >/dev/null 2>&1; then
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda wait function-active --function-name "$FUNCTION_NAME"
fi

if AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws lambda get-function-url-config --function-name "$FUNCTION_NAME" >/dev/null 2>&1; then
  FUNCTION_URL="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws lambda update-function-url-config \
        --function-name "$FUNCTION_NAME" \
        --auth-type "$AUTH_TYPE" \
        --invoke-mode "$FUNCTION_URL_INVOKE_MODE" \
        --query 'FunctionUrl' --output text
  )"
else
  FUNCTION_URL="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws lambda create-function-url-config \
        --function-name "$FUNCTION_NAME" \
        --auth-type "$AUTH_TYPE" \
        --invoke-mode "$FUNCTION_URL_INVOKE_MODE" \
        --query 'FunctionUrl' --output text
  )"
fi

if [[ "$AUTH_TYPE" == "NONE" ]]; then
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda add-permission \
      --function-name "$FUNCTION_NAME" \
      --statement-id "${FUNCTION_NAME}-function-url-public" \
      --action lambda:InvokeFunctionUrl \
      --principal '*' \
      --function-url-auth-type NONE >/dev/null 2>&1 || true

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda add-permission \
      --function-name "$FUNCTION_NAME" \
      --statement-id "${FUNCTION_NAME}-public-invoke" \
      --action lambda:InvokeFunction \
      --principal '*' >/dev/null 2>&1 || true
fi

echo
echo "Deployment complete."
echo "Function ARN: $(
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda get-function --function-name "$FUNCTION_NAME" --query 'Configuration.FunctionArn' --output text
)"
echo "Function URL: $FUNCTION_URL"
