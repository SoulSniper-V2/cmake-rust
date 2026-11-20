cmake_minimum_required(VERSION 4.1)
set(ENV{ACME_ROOT} "data/root")
find_package(Acme CONFIG)
message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
