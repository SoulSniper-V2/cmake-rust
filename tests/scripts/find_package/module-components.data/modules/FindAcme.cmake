
    if(Acme_FIND_MODULE)
      message("${CMAKE_FIND_PACKAGE_NAME}|${Acme_FIND_COMPONENTS}|${Acme_FIND_REQUIRED_first}|${Acme_FIND_REQUIRED_second}|${Acme_FIND_REQUIRED_third}|${Acme_FIND_REQUIRED}|${Acme_FIND_QUIETLY}|${Acme_FIND_REGISTRY_VIEW}")
      set(Acme_FOUND TRUE)
    endif()
