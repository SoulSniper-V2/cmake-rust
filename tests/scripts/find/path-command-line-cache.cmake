cmake_minimum_required(VERSION 4.1)
find_path(SAVED NAMES absent NO_DEFAULT_PATH)
message("${SAVED}|$CACHE{SAVED}")
