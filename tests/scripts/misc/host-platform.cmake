cmake_host_system_information(RESULT result QUERY OS_NAME OS_RELEASE OS_VERSION OS_PLATFORM HOSTNAME FQDN)
message("${result}")
