slint::slint! {
    export component OpacityProbe inherits Window {
        width: 80px; height: 60px; background: black;
        in-out property <float> alpha: 0;
        in-out property <color> child-color: red;
        pure callback paint-color(color) -> color;
        Rectangle {
            x: 0px; y: 0px; width: 60px; height: 40px;
            opacity: root.alpha;
            Rectangle { x: 0px; y: 0px; width: 40px; height: 40px; background: root.paint-color(root.child-color); }
            Rectangle { x: 20px; y: 0px; width: 40px; height: 40px; background: root.paint-color(root.child-color); }
            // Overflow must also clear on hide and redraw on reveal.
            Rectangle { x: 60px; y: 0px; width: 20px; height: 40px; background: root.paint-color(root.child-color); }
        }
    }
}
