interface WsResponse {
  name: string;
  html: string;
}

const main = async () => {
  // const root = document.getElementById("root")!;
  const socket = new WebSocket("/refresh-ws");
  socket.onmessage = (event) => {
    console.log(event);
    window.location.reload();
  };
};

main();
