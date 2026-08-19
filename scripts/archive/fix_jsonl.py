import json
import sys

def fix_jsonl(filename):
    valid_lines = []
    broken_count = 0
    with open(filename, 'r', errors='ignore') as f:
        for i, line in enumerate(f):
            if not line.strip():
                continue
            try:
                json.loads(line)
                valid_lines.append(line)
            except json.JSONDecodeError as e:
                print(f"Line {i+1} broken: {e}")
                broken_count += 1
    
    if broken_count > 0:
        with open(filename, 'w') as f:
            for line in valid_lines:
                f.write(line)
        print(f"Fixed {filename}, removed {broken_count} broken lines.")
    else:
        print("No broken lines found.")

fix_jsonl('.beads/issues.jsonl')