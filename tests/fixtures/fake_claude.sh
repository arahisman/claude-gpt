#!/bin/sh
{
  for argument in "$@"; do
    printf 'arg=%s\n' "$argument"
  done
  printf 'base_url=%s\n' "$ANTHROPIC_BASE_URL"
  printf 'auth_token=%s\n' "$ANTHROPIC_AUTH_TOKEN"
  printf 'use_gateway=%s\n' "$CLAUDE_CODE_USE_GATEWAY"
  printf 'model_discovery=%s\n' "$CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"
  printf 'allow_loopback=%s\n' "$CLAUDE_GATEWAY_ALLOW_LOOPBACK"
  printf 'api_key=%s\n' "$ANTHROPIC_API_KEY"
  printf 'oauth_token=%s\n' "$CLAUDE_CODE_OAUTH_TOKEN"
} > "$FAKE_CLAUDE_CAPTURE"

if [ -n "$FAKE_CLAUDE_WAIT_FILE" ]; then
  while [ ! -e "$FAKE_CLAUDE_WAIT_FILE" ]; do
    sleep 0.05
  done
fi

exit "${FAKE_CLAUDE_EXIT_CODE:-0}"
