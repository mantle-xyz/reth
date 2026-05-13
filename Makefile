# Thin wrapper — forwards all targets to justfile
# Keeps `make build` / `make pr` etc. working for backward compatibility

.DEFAULT_GOAL := default

default:
	@just --list

%:
	@just $@
