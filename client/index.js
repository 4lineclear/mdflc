var socket = new WebSocket("/refresh-ws");
socket.onmessage = function () {
  window.location.reload();
};
socket.onclose = function () {
  window.location.reload();
};
