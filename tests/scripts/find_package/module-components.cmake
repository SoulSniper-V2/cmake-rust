cmake_minimum_required(VERSION 4.1)
set(CMAKE_MODULE_PATH "data/modules")
    set(CMAKE_FIND_PACKAGE_NAME old)
    find_package(Acme MODULE QUIET REQUIRED first COMPONENTS second OPTIONAL_COMPONENTS third REGISTRY_VIEW HOST)
    message("caller=${CMAKE_FIND_PACKAGE_NAME}|${Acme_FIND_COMPONENTS}|${Acme_FIND_REQUIRED}")

message("found=${Acme_FOUND}|dir=${Acme_DIR}|config=${Acme_CONFIG}|considered=${Acme_CONSIDERED_CONFIGS}|versions=${Acme_CONSIDERED_VERSIONS}|origin=${origin}")
