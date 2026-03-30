#!/usr/bin/env bash
# Scripted asciinema demo for ookcite-mcp.
# Usage: ./demo/script.sh
# Output: demo/ookcite-demo.cast
set -euo pipefail

CAST_FILE="demo/ookcite-demo.cast"
DEMO_SCRIPT="demo/_run_demo.sh"
mkdir -p demo

cat > "$DEMO_SCRIPT" << 'DEMO_EOF'
#!/usr/bin/env bash

type_text() {
  local text="$1"
  for (( i=0; i<${#text}; i++ )); do
    printf '%s' "${text:$i:1}"
    sleep 0.0$(( RANDOM % 8 + 3 ))
  done
}

print_line() {
  echo "$1"
  sleep 0.03
}

clear

echo -e "\e[1;36m  OokCite MCP -- Citation validation for LLMs\e[0m"
echo ""
sleep 1

# Scene 1: validate_doi
echo -ne "\e[1;36mclaude\e[0m > "
type_text "Is 10.1038/187493a0 a real paper?"
echo ""
sleep 0.5

echo -e "\e[34m  ookcite - validate_doi\e[0m(doi: \"10.1038/187493a0\")"
sleep 0.3
print_line "  VALID"
print_line "  DOI: 10.1038/187493a0"
print_line "  Title: Stimulated Optical Radiation in Ruby"
print_line "  Authors: Maiman"
print_line "  Year: 1960"
print_line "  Journal: Nature"
print_line "  Volume: 187"
print_line "  Issue: 4736"
sleep 1.5

echo ""
echo -e "Yes -- that is \e[1m\"Stimulated Optical Radiation in Ruby\"\e[0m by Theodore"
echo "Maiman (1960), published in Nature. This is the landmark paper"
echo "reporting the first working laser."
sleep 3

# Scene 2: format_citation
echo ""
echo -ne "\e[1;36mclaude\e[0m > "
type_text "Format it in IEEE style"
echo ""
sleep 0.5

echo -e "\e[34m  ookcite - format_citation\e[0m(doi: \"10.1038/187493a0\", style: \"ieee\")"
sleep 0.3
print_line "  In-text: [1]"
print_line "  Reference: T. H. Maiman, \"Stimulated optical radiation in ruby,\""
print_line "  Nature, vol. 187, no. 4736, pp. 493-494, Aug. 1960."
sleep 1.5

echo ""
echo "Here is the IEEE-formatted citation:"
echo ""
echo -e "  \e[1m[1]\e[0m T. H. Maiman, \"Stimulated optical radiation in ruby,\""
echo "  Nature, vol. 187, no. 4736, pp. 493-494, Aug. 1960."
sleep 3

# Scene 3: catch hallucination
echo ""
echo -ne "\e[1;36mclaude\e[0m > "
type_text "What about 10.1234/fake-hallucinated-paper?"
echo ""
sleep 0.5

echo -e "\e[34m  ookcite - validate_doi\e[0m(doi: \"10.1234/fake-hallucinated-paper\")"
sleep 0.3
print_line "  INVALID: DOI 10.1234/fake-hallucinated-paper not found"
print_line "  in CrossRef. This citation may represent a hallucination."
sleep 1.5

echo ""
echo -e "\e[33mThat DOI does not exist.\e[0m I will not include it as a reference."
sleep 3

# Outro
echo ""
echo -e "\e[1;36m  ookcite.turtletech.us\e[0m -- Install: npx @turtletech/ookcite-mcp setup"
echo ""
sleep 3
DEMO_EOF

chmod +x "$DEMO_SCRIPT"

echo "Recording demo..."
asciinema rec --cols 80 --rows 24 \
  --title "OokCite MCP -- Citation validation for LLMs" \
  --command "$DEMO_SCRIPT" \
  --overwrite \
  "$CAST_FILE"

rm -f "$DEMO_SCRIPT"
echo ""
echo "Done! Preview with: asciinema play $CAST_FILE"
