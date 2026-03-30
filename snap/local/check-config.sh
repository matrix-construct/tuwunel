#!/bin/sh

if [ ! -f "$SNAP_COMMON/tuwunel.toml" ]; then
    echo "Configuration file not found at $SNAP_COMMON/tuwunel.toml"
    echo "Please initialize the configuration by running:"
    echo "  sudo tuwunel.init"
    sleep 60
    exit 1
fi

exec "$@"
