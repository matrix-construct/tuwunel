# Setting up Appservices

## Getting help

If you run into any problems while setting up an Appservice: ask us in
[#tuwunel:grin.hu](https://matrix.to/#/#tuwunel:grin.hu) or
[open an issue on GitHub](https://github.com/matrix-construct/tuwunel/issues/new).

## Set up the appservice - general instructions

Follow whatever instructions are given by the appservice. This usually includes
downloading, changing its config (setting domain, homeserver url, port etc.) and
later starting it.

At some point the appservice guide should ask you to add a registration yaml
file to the homeserver. In Synapse you would do this by adding the path to the
homeserver.yaml, but in tuwunel you can do this from within Matrix:

First, go into the `#admins` room of your homeserver. The first person that
registered on the homeserver automatically joins it. Then send a message into
the room like this:

    !admin appservices register
    ```
    paste
    the
    contents
    of
    the
    yaml
    registration
    here
    ```

You can confirm it worked by sending a message like this:
`!admin appservices list`

The server bot should answer with `Appservices (1): your-bridge`

Then you are done. Tuwunel will send messages to the appservices and the
appservice can send requests to the homeserver. You don't need to restart
tuwunel, but if it doesn't work, restarting while the appservice is running
could help.

## Appservice-specific instructions

### Remove an appservice

To remove an appservice go to your admin room and execute

`!admin appservices unregister <name>`

where `<name>` one of the output of `appservices list`.
