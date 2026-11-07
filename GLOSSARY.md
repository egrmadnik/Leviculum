# LNode

Standalone Reticulum/LXMF firmware for nRF52840 boards, built from the `leviculum-nrf` crate.

## Language

**LNode**:
The firmware family that runs a Reticulum node with LXMF messaging on one of the supported boards.
_Avoid_: lnode firmware, nrf node

**Board**:
One supported hardware unit (T114, RAK4631, Solarnode, Xiaokit) and its pin and peripheral definitions.
_Avoid_: target, BSP, hardware unit

**Reticulum module**:
The shared part of an LNode that owns identity, storage, interfaces and transport.
_Avoid_: core loop, stack

**LXMF service**:
The shared part of an LNode that registers a delivery destination and sends and receives LXMF messages.
_Avoid_: messenger, LXMF layer

**Delivery destination**:
The LXMF destination an LNode registers under a display name so that peers can send it messages.
_Avoid_: inbox address, LXMF endpoint

**App task**:
A per-application part of an LNode that consumes LXMF events and issues LXMF commands. Its message handling runs as a step in the main loop; any hardware it drives runs as its own Embassy task.
_Avoid_: handler, callback

**Peripheral**:
An optional board-specific capability such as battery, GNSS or display that is plugged into an LNode.
_Avoid_: sensor, driver

**Echo app**:
An App task that replies to every received LXMF message with its own text prefixed by "Echo: ".
_Avoid_: lxmf_echo, auto_echo

**Blink app**:
An App task that blinks an LED at a frequency a remote node sets by LXMF message, and acknowledges each change by reply.
_Avoid_: LED app, blinker

**Blink frequency**:
The rate, in hertz, at which the Blink app flashes its LED; zero means the LED is off.
_Avoid_: blink rate, blink period

**Command**:
A text message a remote node sends to an app to change a setting, such as `blink 2`; it is validated in full before any of it is applied.
_Avoid_: instruction, request

**Report**:
A message an app sends to a remote node to say what state it is in, either as the reply to a command or on its own.
_Avoid_: telemetry, notification
