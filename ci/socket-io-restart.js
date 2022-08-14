let createServer = require('http').createServer;
let server = createServer();
const io = require('socket.io')(server);

console.log('Started');
var callback = client => {
    console.log('Connected!');
    client.on('restart_server', () => {
        console.log('will restart in 2s');
        io.close();
        setTimeout(() => {
            console.log("do restart")
            server = createServer();
            server.listen(4205);
            io.attach(server);
        }, 2000)
    });
};
io.on('connection', callback);
io.of('/admin').on('connection', callback);
// the socket.io client runs on port 4201
server.listen(4205);
