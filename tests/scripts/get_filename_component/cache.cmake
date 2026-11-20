get_filename_component(out /x/one NAME CACHE)
get_filename_component(out /y/two NAME CACHE)
message("normal=${out} cache=$CACHE{out}")
