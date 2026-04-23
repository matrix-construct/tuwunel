# Voice and Videotelephony Systems

There are two ways of setting up voice and video calling for use with Matrix:

- Legacy Calling (using a TURN backend).
- MatrixRTC/Element Call (using a
  [Livekit](https://github.com/livekit/livekit) backend).

Which of these is right for your homeserver largely depends on your preferred
client. Different clients support different calling methods, but the majority
of maintained clients that support calling are moving towards using MatrixRTC.
It is also possible to use legacy calls and the newer MatrixRTC concurrently.

To set up legacy calling, see the [TURN documentation](calls/turn.md). Note:
if you are also setting up MatrixRTC, additionally review the
[External TURN Integration](calls/matrix_rtc.md#external-turn-integration)
section of the MatrixRTC documentation.

To set up MatrixRTC/Element Call, see the
[MatrixRTC documentation](calls/matrix_rtc.md).
