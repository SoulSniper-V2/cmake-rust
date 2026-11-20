cmake_policy(SET CMP0109 OLD)

find_program(tool NAMES needle.tool HINTS data/nonexec data/exec NO_DEFAULT_PATH)
message("${tool}")
