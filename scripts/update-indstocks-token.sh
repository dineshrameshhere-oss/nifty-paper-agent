#!/data/data/com.termux/files/usr/bin/bash
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
env_file="$root/.env"
temp_file="$env_file.tmp.$$"
umask 077

read -r -s -p 'New INDstocks access token: ' token; printf '\n'
if [ -z "$token" ] || [[ "$token" == *$'\n'* || "$token" == *$'\r'* ]]; then
  printf 'Token must be a non-empty single-line value. .env was not changed.\n' >&2
  exit 1
fi

if [ -f "$env_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      INDSTOCKS_ACCESS_TOKEN=*) ;;
      *) printf '%s\n' "$line" >> "$temp_file" ;;
    esac
  done < "$env_file"
fi
printf 'INDSTOCKS_ACCESS_TOKEN=%s\n' "$token" >> "$temp_file"
mv "$temp_file" "$env_file"
chmod 600 "$env_file"
unset token
printf 'Updated the INDstocks access token in .env. Other entries were kept.\n'
