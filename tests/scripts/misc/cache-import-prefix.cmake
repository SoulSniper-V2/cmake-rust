execute_process(COMMAND /bin/sh -c "printf 'k:STRING=value\nempty:STRING=\nother:STRING=ignored\n' > CMakeCache.txt")
set(p_empty sentinel)
load_cache(. READ_WITH_PREFIX p_ k empty)
message("${p_k}|${p_empty}|${p_other}")
