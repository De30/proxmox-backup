Ext.define('PBS.AccessControlPanel', {
    extend: 'Ext.tab.Panel',
    alias: 'widget.pbsAccessControlPanel',
    mixins: ['Proxmox.Mixin.CBind'],

    title: gettext('Access Control'),

    border: false,
    defaults: {
	border: false,
    },

    items: [
	{
	    xtype: 'pbsUserView',
	    title: gettext('User Management'),
	    itemId: 'users',
	    iconCls: 'fa fa-user',
	},
	{
	    xtype: 'pbsTokenView',
	    title: gettext('API Token'),
	    itemId: 'apitokens',
	    iconCls: 'fa fa-user-o',
	},
	{
	    xtype: 'pbsACLView',
	    title: gettext('Permissions'),
	    itemId: 'permissions',
	    iconCls: 'fa fa-unlock',
	},
    ],

});