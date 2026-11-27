get_filename_component(unknown "~cmake_audit_invalid:user/missing/../item" ABSOLUTE)
get_filename_component(mixed [=[/\root/subdir/../item]=] ABSOLUTE)
set(ENV{HOME} "relative/home")
get_filename_component(home "~/item" ABSOLUTE)
message("unknown: [${unknown}], mixed: [${mixed}], home: [${home}]")

set(ENV{HOME} "~root/sdk")
get_filename_component(nested_home "~/item" ABSOLUTE)
message("nested home: [${nested_home}]")
