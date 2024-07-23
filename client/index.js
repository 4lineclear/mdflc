var socket = new WebSocket("/refresh-ws");
socket.onmessage = function (message) {
  if (message.data === "refresh") {
    window.location.reload();
  }
};
