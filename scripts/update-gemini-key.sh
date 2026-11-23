#!/data/data/com.termux/files/usr/bin/bash
set -eu
root="$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)"
env_file="$root/.env"
temp_file="$env_file.tmp.$$"
umask 077

read -r -s -p 'Google Gemini API key: ' gemini_key; printf '\n'
if [ -z "$gemini_key" ] || [[ "$gemini_key" == *$'\n'* || "$gemini_key" == *$'\r'* ]]; then
  printf 'Key must be a non-empty single-line value. .env was not changed.\n' >&2
  exit 1
fi

: > "$temp_file"
if [ -f "$env_file" ]; then
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
      GEMINI_API_KEY=*|GROQ_API_KEY=*) ;;
      *) printf '%s\n' "$line" >> "$temp_file" ;;
    esac
  done < "$env_file"
fi
printf 'GEMINI_API_KEY=%s\n' "$gemini_key" >> "$temp_file"
mv "$temp_file" "$env_file"
chmod 600 "$env_file"
unset gemini_key
printf 'Saved Gemini API key to .env. Values were not displayed.\n'
