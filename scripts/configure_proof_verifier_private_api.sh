#!/usr/bin/env bash
set -euo pipefail

AWS_PROFILE="${AWS_PROFILE:-}"
AWS_REGION="${AWS_REGION:-}"
FUNCTION_NAME="${FUNCTION_NAME:-}"
PRIVATE_API_NAME="${PRIVATE_API_NAME:-}"
PRIVATE_STAGE_NAME="${PRIVATE_STAGE_NAME:-}"
VPC_ID="${VPC_ID:-}"
VPC_CIDRS="${VPC_CIDRS:-}"
VPCE_SUBNET_IDS="${VPCE_SUBNET_IDS:-}"
VPCE_SG_NAME="${VPCE_SG_NAME:-}"

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
require_cmd python3

require_env AWS_PROFILE
require_env AWS_REGION
require_env FUNCTION_NAME
require_env PRIVATE_API_NAME
require_env PRIVATE_STAGE_NAME
require_env VPC_ID
require_env VPC_CIDRS
require_env VPCE_SUBNET_IDS
require_env VPCE_SG_NAME

ACCOUNT_ID="$(
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws sts get-caller-identity --query Account --output text
)"

LAMBDA_ARN="$(
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda get-function --function-name "$FUNCTION_NAME" --query 'Configuration.FunctionArn' --output text
)"

ensure_vpce_security_group() {
  local sg_id
  sg_id="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws ec2 describe-security-groups \
        --filters "Name=vpc-id,Values=$VPC_ID" "Name=group-name,Values=$VPCE_SG_NAME" \
        --query 'SecurityGroups[0].GroupId' --output text 2>/dev/null || true
  )"

  if [[ -z "$sg_id" || "$sg_id" == "None" ]]; then
    sg_id="$(
      AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
        aws ec2 create-security-group \
          --group-name "$VPCE_SG_NAME" \
          --description "Security group for proof verifier private execute-api endpoint" \
          --vpc-id "$VPC_ID" \
          --query 'GroupId' --output text
    )"
  fi

  IFS=',' read -r -a cidr_list <<< "$VPC_CIDRS"
  for cidr in "${cidr_list[@]}"; do
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws ec2 authorize-security-group-ingress \
        --group-id "$sg_id" \
        --ip-permissions "[{\"IpProtocol\":\"tcp\",\"FromPort\":443,\"ToPort\":443,\"IpRanges\":[{\"CidrIp\":\"${cidr}\",\"Description\":\"VPC HTTPS to private execute-api\"}]}]" \
        >/dev/null 2>&1 || true
  done

  printf '%s\n' "$sg_id"
}

ensure_execute_api_vpce() {
  local vpce_id
  local sg_id="$1"

  vpce_id="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws ec2 describe-vpc-endpoints \
        --filters "Name=vpc-id,Values=$VPC_ID" "Name=service-name,Values=com.amazonaws.${AWS_REGION}.execute-api" \
        --query 'VpcEndpoints[0].VpcEndpointId' --output text 2>/dev/null || true
  )"

  if [[ -z "$vpce_id" || "$vpce_id" == "None" ]]; then
    vpce_id="$(
      AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
        aws ec2 create-vpc-endpoint \
          --vpc-id "$VPC_ID" \
          --service-name "com.amazonaws.${AWS_REGION}.execute-api" \
          --vpc-endpoint-type Interface \
          --private-dns-enabled \
          --subnet-ids ${VPCE_SUBNET_IDS//,/ } \
          --security-group-ids "$sg_id" \
          --query 'VpcEndpoint.VpcEndpointId' --output text
    )"
  fi

  for _ in $(seq 1 60); do
    local state
    state="$(
      AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
        aws ec2 describe-vpc-endpoints \
          --vpc-endpoint-ids "$vpce_id" \
          --query 'VpcEndpoints[0].State' --output text
    )"
    if [[ "$state" == "available" ]]; then
      break
    fi
    sleep 5
  done

  printf '%s\n' "$vpce_id"
}

render_api_policy() {
  local vpce_id="$1"
  python3 - "$vpce_id" <<'PY'
import json
import sys

vpce_id = sys.argv[1]
policy = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Sid": "AllowInvokeFromSpecificVpce",
            "Effect": "Allow",
            "Principal": "*",
            "Action": "execute-api:Invoke",
            "Resource": "execute-api:/*",
            "Condition": {"StringEquals": {"aws:SourceVpce": vpce_id}},
        },
        {
            "Sid": "DenyInvokeOutsideSpecificVpce",
            "Effect": "Deny",
            "Principal": "*",
            "Action": "execute-api:Invoke",
            "Resource": "execute-api:/*",
            "Condition": {"StringNotEquals": {"aws:SourceVpce": vpce_id}},
        },
    ],
}
print(json.dumps(policy, separators=(",", ":")))
PY
}

ensure_private_rest_api() {
  local vpce_id="$1"
  local api_id
  local policy

  api_id="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws apigateway get-rest-apis \
        --query "items[?name=='$PRIVATE_API_NAME'].id | [0]" --output text 2>/dev/null || true
  )"

  policy="$(render_api_policy "$vpce_id")"

  if [[ -z "$api_id" || "$api_id" == "None" ]]; then
    api_id="$(
      AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
        aws apigateway create-rest-api \
          --name "$PRIVATE_API_NAME" \
          --endpoint-configuration "types=PRIVATE,vpcEndpointIds=${vpce_id}" \
          --policy "$policy" \
          --query 'id' --output text
    )"
  else
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws apigateway update-rest-api \
        --rest-api-id "$api_id" \
        --patch-operations \
          "op=replace,path=/policy,value=$policy" \
          "op=replace,path=/endpointConfiguration/vpcEndpointIds/$vpce_id,value=$vpce_id" \
        >/dev/null 2>&1 || true
  fi

  printf '%s\n' "$api_id"
}

get_or_create_resource() {
  local api_id="$1"
  local parent_id="$2"
  local path_part="$3"
  local resource_id

  resource_id="$(
    AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
      aws apigateway get-resources --rest-api-id "$api_id" \
        --query "items[?parentId=='$parent_id' && pathPart=='$path_part'].id | [0]" \
        --output text 2>/dev/null || true
  )"

  if [[ -z "$resource_id" || "$resource_id" == "None" ]]; then
    resource_id="$(
      AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
        aws apigateway create-resource \
          --rest-api-id "$api_id" \
          --parent-id "$parent_id" \
          --path-part "$path_part" \
          --query 'id' --output text
    )"
  fi

  printf '%s\n' "$resource_id"
}

put_lambda_proxy_method() {
  local api_id="$1"
  local resource_id="$2"
  local http_method="$3"

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws apigateway put-method \
      --rest-api-id "$api_id" \
      --resource-id "$resource_id" \
      --http-method "$http_method" \
      --authorization-type NONE \
      >/dev/null 2>&1 || true

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws apigateway put-integration \
      --rest-api-id "$api_id" \
      --resource-id "$resource_id" \
      --http-method "$http_method" \
      --type AWS_PROXY \
      --integration-http-method POST \
      --uri "arn:aws:apigateway:${AWS_REGION}:lambda:path/2015-03-31/functions/${LAMBDA_ARN}/invocations" \
      >/dev/null
}

ensure_lambda_permission_for_apigw() {
  local api_id="$1"
  local statement_id="${PRIVATE_API_NAME//[^a-zA-Z0-9]/}-invoke"
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda add-permission \
      --function-name "$FUNCTION_NAME" \
      --statement-id "$statement_id" \
      --action lambda:InvokeFunction \
      --principal apigateway.amazonaws.com \
      --source-arn "arn:aws:execute-api:${AWS_REGION}:${ACCOUNT_ID}:${api_id}/*/*/*" \
      >/dev/null 2>&1 || true
}

ensure_public_function_url_is_open() {
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda update-function-url-config \
      --function-name "$FUNCTION_NAME" \
      --auth-type NONE \
      >/dev/null

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda add-permission \
      --function-name "$FUNCTION_NAME" \
      --statement-id "${FUNCTION_NAME}-function-url-public" \
      --action lambda:InvokeFunctionUrl \
      --principal '*' \
      --function-url-auth-type NONE \
      >/dev/null 2>&1 || true

  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda add-permission \
      --function-name "$FUNCTION_NAME" \
      --statement-id "${FUNCTION_NAME}-function-url-public-invoke" \
      --action lambda:InvokeFunction \
      --principal '*' \
      >/dev/null 2>&1 || true
}

get_function_url() {
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws lambda get-function-url-config \
      --function-name "$FUNCTION_NAME" \
      --query 'FunctionUrl' --output text 2>/dev/null || true
}

vpce_sg_id="$(ensure_vpce_security_group)"
vpce_id="$(ensure_execute_api_vpce "$vpce_sg_id")"
api_id="$(ensure_private_rest_api "$vpce_id")"

root_id="$(
  AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
    aws apigateway get-resources --rest-api-id "$api_id" \
      --query 'items[?path==`/`].id | [0]' --output text
)"
proxy_id="$(get_or_create_resource "$api_id" "$root_id" "{proxy+}")"

put_lambda_proxy_method "$api_id" "$root_id" "ANY"
put_lambda_proxy_method "$api_id" "$proxy_id" "ANY"
ensure_lambda_permission_for_apigw "$api_id"

AWS_PROFILE="$AWS_PROFILE" AWS_REGION="$AWS_REGION" \
  aws apigateway create-deployment \
    --rest-api-id "$api_id" \
    --stage-name "$PRIVATE_STAGE_NAME" \
    >/dev/null

ensure_public_function_url_is_open

function_url="$(get_function_url)"

cat <<EOF
Private API configuration complete.
Function name:   $FUNCTION_NAME
Private API ID:  $api_id
Private stage:   $PRIVATE_STAGE_NAME
VPCE ID:         $vpce_id
VPCE SG:         $vpce_sg_id

Public base URL:
${function_url}

Private VPC base URLs:
https://${api_id}.execute-api.${AWS_REGION}.amazonaws.com/${PRIVATE_STAGE_NAME}
https://${api_id}-${vpce_id}.execute-api.${AWS_REGION}.amazonaws.com/${PRIVATE_STAGE_NAME}
EOF
