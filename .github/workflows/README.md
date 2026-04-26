## Overview

The release pipeline `Main` (main.yml) and its subroutines defined in the other yamls form a high-level
description for the underlying self-hosted build system in  `/docker`. In other words, this is a sort of
terminal, a "thin-client" with a display and a keyboard for our docker mainframe. We minimize
vendor-lockin and duplication with other services by limiting everything here to only what is
essential for driving the docker builder. See: [documentation](../../docs/development/testing/workflows.md)
