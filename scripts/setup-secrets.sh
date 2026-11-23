#!/data/data/com.termux/files/usr/bin/bash
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
env_file="$root/.env"
umask 077

read -r -s -p 'INDstocks access token: ' access_token; printf '\n'
read -r -s -p 'Google Gemini API key: ' gemini_key; printf '\n'
for value in "$access_token" "$gemini_key"; do
  if [ -z "$value" ] || [[ "$value" == *$'\n'* || "$value" == *$'\r'* ]]; then
    printf 'Keys must be non-empty single-line values. No .env file was written.\n' >&2
    exit 1
  fi
done

printf 'INDSTOCKS_ACCESS_TOKEN=%s\nGEMINI_API_KEY=%s\n' \
  "$access_token" "$gemini_key" > "$env_file"
chmod 600 "$env_file"
unset access_token gemini_key
printf 'Saved credentials to .env with owner-only permissions. Values were not displayed.\n'
